//! Byte-compatible generation of the `sqlite_stat4` table for `ANALYZE`.
//!
//! This is a faithful port of SQLite's STAT4 accumulator (the `stat_init`,
//! `stat_push`, `samplePushPrevious`, `sampleInsert` and `stat_get` functions in
//! `analyze.c`, compiled with `SQLITE_ENABLE_STAT4`). Given the entries of one
//! index in index-storage order, it reproduces SQLite's reservoir/periodic
//! sampling — including the pseudo-random `iPrn`/`iHash` tie-breaker seeded from
//! the column count and estimated row count — so the emitted `neq`/`nlt`/`ndlt`
//! integer lists and `sample` records match the real `sqlite3` (STAT4 build)
//! byte-for-byte.
//!
//! The caller (see `Connection::exec_analyze`) is responsible for turning each
//! index into the [`Stat4Entry`] list this module consumes and for serializing
//! the resulting [`Stat4Sample`]s into `sqlite_stat4` rows.

use crate::format::record::encode_record;
use crate::value::Value;
use alloc::string::ToString;
use alloc::vec::Vec;

/// Default number of samples SQLite keeps per index (`SQLITE_STAT4_SAMPLES`).
const STAT4_SAMPLES: usize = 24;

/// One entry of an index, in index-storage order, as fed to the accumulator.
///
/// `sample` is the ordered list of the index's *sample columns* — the key
/// columns followed by the trailing rowid (rowid tables) or the primary-key
/// columns (WITHOUT ROWID tables). This is exactly what SQLite serializes into
/// the `sample` column, so it is passed through [`encode_record`] verbatim.
///
/// (SQLite also carries the row's rowid so `stat_get` can re-seek the table to
/// re-read the sample columns; because we keep those column values here
/// directly, no rowid is needed.)
pub(crate) struct Stat4Entry {
    pub sample: Vec<Value>,
}

/// A finished STAT4 sample, ready to be written as a `sqlite_stat4` row.
pub(crate) struct Stat4Sample {
    pub neq: Vec<u64>,
    pub nlt: Vec<u64>,
    pub ndlt: Vec<u64>,
    pub sample: Vec<u8>,
}

impl Stat4Sample {
    /// Render `neq`/`nlt`/`ndlt` as a space-separated integer string, exactly as
    /// SQLite's `statGet` builds it.
    pub fn stat_string(counts: &[u64]) -> alloc::string::String {
        let mut s = alloc::string::String::new();
        for (i, c) in counts.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(&c.to_string());
        }
        s
    }
}

/// A single collected sample inside the accumulator (`StatSample` in SQLite).
///
/// In addition to the C `StatSample` fields, this carries `sample_values` — the
/// index's sample-column values for the current row. SQLite recovers these by
/// re-seeking the table from the stored rowid at `stat_get` time; we keep them
/// attached so the finished record can be serialized directly.
#[derive(Clone)]
struct Sample {
    an_eq: Vec<u64>,
    an_lt: Vec<u64>,
    an_dlt: Vec<u64>,
    sample_values: Vec<Value>,
    is_psample: bool,
    /// Reason for inclusion (which column's cardinality drove it), for a
    /// non-periodic sample.
    i_col: usize,
    /// Tie-breaker hash.
    i_hash: u32,
}

impl Sample {
    fn new(n_col: usize) -> Self {
        Sample {
            an_eq: alloc::vec![0; n_col],
            an_lt: alloc::vec![0; n_col],
            an_dlt: alloc::vec![0; n_col],
            sample_values: Vec::new(),
            is_psample: false,
            i_col: 0,
            i_hash: 0,
        }
    }
}

/// The STAT4 accumulator: a faithful port of `struct StatAccum` plus its three
/// SQL functions. Fed one index's entries in storage order, then queried for the
/// finished samples.
struct StatAccum {
    n_col: usize,
    n_row: u64,
    n_psample: u64,
    mx_sample: usize,
    i_prn: u32,
    current: Sample,
    /// `aBest[]`: one candidate per column.
    a_best: Vec<Sample>,
    i_min: usize,
    n_max_eq_zero: usize,
    /// `a[]`: the collected samples.
    a: Vec<Sample>,
}

impl StatAccum {
    /// Port of `statInit(N, K, C, L)` with `L`==0 (no scan limit). `n_col` is the
    /// number of sample columns (key + rowid/pk); `n_est` is the estimated row
    /// count (== the actual count here, since we scan the whole index).
    fn new(n_col: usize, n_est: u64) -> Self {
        let mx_sample = STAT4_SAMPLES;
        let n_psample = n_est / (mx_sample as u64 / 3 + 1) + 1;
        // iPrn = 0x689e962d*nCol ^ 0xd0944565*nEst  (all 32-bit wrapping)
        let i_prn =
            0x689e_962du32.wrapping_mul(n_col as u32) ^ 0xd094_4565u32.wrapping_mul(n_est as u32);
        let mut current = Sample::new(n_col);
        // stat_push sets anEq[*]=1 on the very first row; mirror the layout here
        // but the flag is applied in `push`.
        current.i_col = 0;
        let a_best = (0..n_col)
            .map(|i| {
                let mut s = Sample::new(n_col);
                s.i_col = i;
                s
            })
            .collect();
        StatAccum {
            n_col,
            n_row: 0,
            n_psample,
            mx_sample,
            i_prn,
            current,
            a_best,
            i_min: 0,
            n_max_eq_zero: 0,
            a: Vec::new(),
        }
    }

    /// Port of `sampleIsBetterPost`: with `pNew->iCol == pOld->iCol`, compare the
    /// trailing `anEq[]` entries then the hash.
    fn sample_is_better_post(new: &Sample, old: &Sample, n_col: usize) -> bool {
        debug_assert_eq!(new.i_col, old.i_col);
        for i in (new.i_col + 1)..n_col {
            if new.an_eq[i] > old.an_eq[i] {
                return true;
            }
            if new.an_eq[i] < old.an_eq[i] {
                return false;
            }
        }
        new.i_hash > old.i_hash
    }

    /// Port of `sampleIsBetter`.
    fn sample_is_better(new: &Sample, old: &Sample, n_col: usize) -> bool {
        let n_eq_new = new.an_eq[new.i_col];
        let n_eq_old = old.an_eq[old.i_col];
        if n_eq_new > n_eq_old {
            return true;
        }
        if n_eq_new == n_eq_old {
            if new.i_col < old.i_col {
                return true;
            }
            return new.i_col == old.i_col && Self::sample_is_better_post(new, old, n_col);
        }
        false
    }

    /// Port of `sampleInsert`: copy `new` into `a[]`, evicting the least-desirable
    /// sample if full. `n_eq_zero` leading `anEq[]` entries are zeroed.
    fn sample_insert(&mut self, new: &Sample, n_eq_zero: usize) {
        if n_eq_zero > self.n_max_eq_zero {
            self.n_max_eq_zero = n_eq_zero;
        }

        if !new.is_psample {
            debug_assert!(new.an_eq[new.i_col] > 0);
            // Upgrade the highest-priority existing sample that shares this prefix,
            // if any, instead of adding a new one.
            let mut upgrade: Option<usize> = None;
            for i in (0..self.a.len()).rev() {
                if self.a[i].an_eq[new.i_col] == 0 {
                    if self.a[i].is_psample {
                        return;
                    }
                    match upgrade {
                        None => upgrade = Some(i),
                        Some(u) => {
                            if Self::sample_is_better(&self.a[i], &self.a[u], self.n_col) {
                                upgrade = Some(i);
                            }
                        }
                    }
                }
            }
            if let Some(u) = upgrade {
                self.a[u].i_col = new.i_col;
                let col = self.a[u].i_col;
                self.a[u].an_eq[col] = new.an_eq[col];
                self.find_new_min();
                return;
            }
        }

        // Remove sample iMin to make room, if full.
        if self.a.len() >= self.mx_sample {
            // memmove: drop a[iMin], shifting the rest down; the new slot is a[end].
            self.a.remove(self.i_min);
        }

        // Insert the new sample.
        let mut s = new.clone();
        // Zero the first n_eq_zero entries in anEq[].
        for e in s.an_eq.iter_mut().take(n_eq_zero) {
            *e = 0;
        }
        self.a.push(s);

        self.find_new_min();
    }

    /// Port of the `find_new_min:` label — recompute `iMin` when at capacity.
    fn find_new_min(&mut self) {
        if self.a.len() >= self.mx_sample {
            let mut i_min: Option<usize> = None;
            for i in 0..self.a.len() {
                if self.a[i].is_psample {
                    continue;
                }
                match i_min {
                    None => i_min = Some(i),
                    Some(m) => {
                        if Self::sample_is_better(&self.a[m], &self.a[i], self.n_col) {
                            i_min = Some(i);
                        }
                    }
                }
            }
            if let Some(m) = i_min {
                self.i_min = m;
            }
        }
    }

    /// Port of `samplePushPrevious`.
    fn sample_push_previous(&mut self, i_chng: usize) {
        // Push candidates from aBest[] into a[] as needed.
        for i in (i_chng..=(self.n_col.saturating_sub(2))).rev() {
            // guard: the C loop is `for(i=nCol-2; i>=iChng; i--)`; if nCol<2 it
            // does not run.
            if self.n_col < 2 {
                break;
            }
            self.a_best[i].an_eq[i] = self.current.an_eq[i];
            let better = self.a.len() < self.mx_sample
                || Self::sample_is_better(&self.a_best[i], &self.a[self.i_min], self.n_col);
            if better {
                let best = self.a_best[i].clone();
                self.sample_insert(&best, i);
            }
        }

        // Update anEq[] fields of any samples already collected.
        if i_chng < self.n_max_eq_zero {
            for i in (0..self.a.len()).rev() {
                for j in i_chng..self.n_col {
                    if self.a[i].an_eq[j] == 0 {
                        self.a[i].an_eq[j] = self.current.an_eq[j];
                    }
                }
            }
            self.n_max_eq_zero = i_chng;
        }
    }

    /// Port of `stat_push(P, iChng, R)`. `sample_values` are the index's
    /// sample-column values for this row (attached so the record can be built
    /// later without re-seeking the table).
    fn push(&mut self, i_chng: usize, sample_values: Vec<Value>) {
        if self.n_row == 0 {
            for e in self.current.an_eq.iter_mut() {
                *e = 1;
            }
        } else {
            self.sample_push_previous(i_chng);
            for i in 0..i_chng {
                self.current.an_eq[i] += 1;
            }
            for i in i_chng..self.n_col {
                self.current.an_dlt[i] += 1;
                self.current.an_lt[i] += self.current.an_eq[i];
                self.current.an_eq[i] = 1;
            }
        }

        self.n_row += 1;
        self.current.sample_values = sample_values;
        self.i_prn = self.i_prn.wrapping_mul(1103515245).wrapping_add(12345);
        self.current.i_hash = self.i_prn;

        let n_lt = self.current.an_lt[self.n_col - 1];
        // Periodic sample?
        if n_lt / self.n_psample != (n_lt + 1) / self.n_psample {
            self.current.is_psample = true;
            self.current.i_col = 0;
            let cur = self.current.clone();
            self.sample_insert(&cur, self.n_col - 1);
            self.current.is_psample = false;
        }

        // Update aBest[].
        for i in 0..(self.n_col - 1) {
            self.current.i_col = i;
            if i >= i_chng
                || Self::sample_is_better_post(&self.current, &self.a_best[i], self.n_col)
            {
                let mut b = self.current.clone();
                b.i_col = i;
                self.a_best[i] = b;
            }
        }
    }

    /// Port of the tail of `stat_get(STAT_GET_ROWID)`: flush the final
    /// `samplePushPrevious(0)` and produce the finished, rowid-ordered samples.
    fn finish(mut self) -> Vec<Stat4Sample> {
        // stat_get's first ROWID call runs samplePushPrevious(p, 0).
        self.sample_push_previous(0);
        self.a
            .into_iter()
            .map(|s| Stat4Sample {
                neq: s.an_eq,
                nlt: s.an_lt,
                ndlt: s.an_dlt,
                sample: encode_record(&s.sample_values),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// STAT4 *consumption* (planner side): estimate the row count an equality
// constraint selects, faithfully porting SQLite's `initAvgEq`, `whereKeyStats`
// and `whereEqualScanEst` (analyze.c / where.c, `SQLITE_ENABLE_STAT4`).
// ---------------------------------------------------------------------------

/// One decoded `sqlite_stat4` sample for a single index, in `sqlite_stat4`
/// storage order (which is index-key order). `n_lt`/`n_eq`/`n_dlt` are the
/// per-prefix statistics; `sample` are the decoded sample-column values.
pub(crate) struct LoadedSample {
    pub n_lt: Vec<u64>,
    pub n_eq: Vec<u64>,
    pub n_dlt: Vec<u64>,
    pub sample: Vec<Value>,
}

/// All STAT4 data the estimator needs for one index, plus the derived
/// `aAvgEq[]`/`nRowEst0` (`loadStat4` + `initAvgEq`).
struct Stat4Index {
    samples: Vec<LoadedSample>,
    /// `nSampleCol` = key cols + trailing rowid/pk cols (== sample.len()).
    n_sample_col: usize,
    /// `nRowEst0`: non-logarithmic number of rows in the index.
    n_row_est0: u64,
    /// `aAvgEq[]`: average nEq for keys not represented by a sample.
    a_avg_eq: Vec<u64>,
}

impl Stat4Index {
    /// Port of `initAvgEq`: compute `aAvgEq[]` and `nRowEst0` from the loaded
    /// samples and stat1's integer list `[nRow, avgEq_1, …]` (`ai_row_est`, may
    /// be short/empty). `n_key_col` is the number of index key columns (excludes
    /// the trailing rowid/pk).
    fn compute_avg_eq(&mut self, ai_row_est: &[u64], n_key_col: usize) {
        let n_sample = self.samples.len();
        if n_sample == 0 {
            return;
        }
        let final_idx = n_sample - 1;
        let mut n_col = 1usize;
        if self.n_sample_col > 1 {
            n_col = self.n_sample_col - 1;
            if let Some(slot) = self.a_avg_eq.get_mut(n_col) {
                *slot = 1;
            }
        }
        for i_col in 0..n_col {
            let mut n_sample_i = n_sample;
            let n_row: u64;
            let n_dist100: i64;
            let row_est_next = if i_col < n_key_col {
                ai_row_est.get(i_col + 1).copied().unwrap_or(0)
            } else {
                0
            };
            if ai_row_est.is_empty() || i_col >= n_key_col || row_est_next == 0 {
                n_row = self.samples[final_idx].n_lt[i_col];
                n_dist100 = 100 * self.samples[final_idx].n_dlt[i_col] as i64;
                n_sample_i -= 1;
            } else {
                n_row = ai_row_est[0];
                n_dist100 = (100 * ai_row_est[0] as i64) / row_est_next as i64;
            }
            self.n_row_est0 = n_row;

            let mut sum_eq: u64 = 0;
            let mut n_sum100: i64 = 0;
            for i in 0..n_sample_i {
                if i == n_sample - 1
                    || self.samples[i].n_dlt[i_col] != self.samples[i + 1].n_dlt[i_col]
                {
                    sum_eq += self.samples[i].n_eq[i_col];
                    n_sum100 += 100;
                }
            }
            let mut avg_eq: u64 = 0;
            if n_dist100 > n_sum100 && sum_eq < n_row {
                avg_eq = (100 * (n_row - sum_eq)) / (n_dist100 - n_sum100) as u64;
            }
            if avg_eq == 0 {
                avg_eq = 1;
            }
            if let Some(slot) = self.a_avg_eq.get_mut(i_col) {
                *slot = avg_eq;
            }
        }
    }
}

/// Compare a stat4 `sample` record (its first `n` decoded values) against a
/// probe `rec`, applying each column's collation and DESC flag — a value-level
/// port of `sqlite3VdbeRecordCompare` restricted to the leading `n` fields.
fn record_compare(
    sample: &[Value],
    rec: &[Value],
    n: usize,
    colls: &[crate::value::Collation],
    descs: &[bool],
) -> core::cmp::Ordering {
    for i in 0..n {
        let coll = colls.get(i).copied().unwrap_or_default();
        let mut ord = crate::value::cmp_values_coll(&sample[i], &rec[i], coll);
        if descs.get(i).copied().unwrap_or(false) {
            ord = ord.reverse();
        }
        if ord != core::cmp::Ordering::Equal {
            return ord;
        }
    }
    core::cmp::Ordering::Equal
}

/// Port of `whereKeyStats(pIdx, pRec, roundUp=0, aStat)`: binary-search the
/// samples for the `n_field`-field probe `rec` and return `(a0, a1)` where `a1`
/// is the estimated nEq for the probe.
fn where_key_stats(
    idx: &Stat4Index,
    rec: &[Value],
    colls: &[crate::value::Collation],
    descs: &[bool],
    n_field: usize,
) -> (u64, u64) {
    let a_sample = &idx.samples;
    let n_sample = a_sample.len();
    let mut i_col = 0usize;
    let mut i_min = 0i64;
    let mut i_sample = (n_sample * n_field) as i64;
    let mut i_lower: u64 = 0;
    let mut res;
    loop {
        let i_test = (i_min + i_sample) / 2;
        let i_samp = (i_test / n_field as i64) as usize;
        let n = if i_samp > 0 {
            let mut nn = (i_test as usize % n_field) + 1;
            while nn < n_field {
                if a_sample[i_samp - 1].n_lt[nn - 1] != a_sample[i_samp].n_lt[nn - 1] {
                    break;
                }
                nn += 1;
            }
            nn
        } else {
            i_test as usize + 1
        };
        res = match record_compare(&a_sample[i_samp].sample, rec, n, colls, descs) {
            core::cmp::Ordering::Less => -1i32,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        };
        if res < 0 {
            i_lower = a_sample[i_samp].n_lt[n - 1] + a_sample[i_samp].n_eq[n - 1];
            i_min = i_test + 1;
        } else if res == 0 && n < n_field {
            i_lower = a_sample[i_samp].n_lt[n - 1];
            i_min = i_test + 1;
            res = -1;
        } else {
            i_sample = i_test;
            i_col = n - 1;
        }
        if res == 0 || i_min >= i_sample {
            break;
        }
    }
    let i = (i_sample / n_field as i64) as usize;

    if res == 0 {
        (a_sample[i].n_lt[i_col], a_sample[i].n_eq[i_col])
    } else {
        let i_upper = if i >= n_sample {
            idx.n_row_est0
        } else {
            a_sample[i].n_lt[i_col]
        };
        let i_gap = i_upper.saturating_sub(i_lower);
        let i_gap = i_gap / 3; // roundUp = 0
        let a1 = idx.a_avg_eq.get(n_field - 1).copied().unwrap_or(1);
        (i_lower + i_gap, a1)
    }
}

/// Port of `whereEqualScanEst`: estimate the number of rows selected by the
/// equality prefix `rec` (all `rec.len()` fields equality-constrained) using
/// the index's stat4 samples. Returns `None` when no estimate is possible (no
/// samples). `n_key_col` = number of index key columns; `ai_row_est` = stat1's
/// integer list for this index (`[nRow, avgEq_1, …]`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn equal_scan_est(
    samples: Vec<LoadedSample>,
    n_sample_col: usize,
    n_key_col: usize,
    ai_row_est: &[u64],
    rec: &[Value],
    colls: &[crate::value::Collation],
    descs: &[bool],
) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let n_field = rec.len().min(n_sample_col);
    if n_field == 0 {
        return None;
    }
    let n_row0 = ai_row_est.first().copied().unwrap_or(0);
    let mut idx = Stat4Index {
        samples,
        n_sample_col,
        n_row_est0: n_row0,
        a_avg_eq: alloc::vec![1; n_sample_col],
    };
    idx.compute_avg_eq(ai_row_est, n_key_col);
    // whereEqualScanEst: if nEq >= nColumn the estimate is 1.
    if n_field >= n_sample_col {
        return Some(1);
    }
    let (_, a1) = where_key_stats(&idx, rec, colls, descs, n_field);
    Some(a1)
}

/// Run the STAT4 accumulator over one index's `entries` (already in index-storage
/// order) and return the finished samples in `sqlite_stat4` row order. `n_col` is
/// the number of sample columns (key + rowid/pk); `n_col_test` is the number of
/// leading columns tested for a change between consecutive entries (SQLite's
/// `nColTest`). Returns an empty vector when there are no entries (an empty index
/// contributes no `sqlite_stat4` rows).
pub(crate) fn collect_samples(
    entries: &[Stat4Entry],
    n_col: usize,
    n_col_test: usize,
    cmp: impl Fn(&[Value], &[Value], usize) -> core::cmp::Ordering,
) -> Vec<Stat4Sample> {
    if entries.is_empty() {
        return Vec::new();
    }
    let mut acc = StatAccum::new(n_col, entries.len() as u64);
    let mut prev: Option<&Stat4Entry> = None;
    for e in entries {
        // iChng = leftmost tested column that differs from the previous entry.
        // If none differ (all tested columns equal), iChng = n_col_test.
        let i_chng = match prev {
            None => 0,
            Some(p) => {
                let mut c = n_col_test;
                for i in 0..n_col_test {
                    if cmp(&p.sample, &e.sample, i + 1) != core::cmp::Ordering::Equal {
                        c = i;
                        break;
                    }
                }
                c
            }
        };
        acc.push(i_chng, e.sample.clone());
        prev = Some(e);
    }
    acc.finish()
}
