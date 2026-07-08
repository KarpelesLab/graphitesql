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
