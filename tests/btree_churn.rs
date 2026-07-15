//! Regression: robust table b-tree page splitting under heavy insert/delete/
//! update churn.
//!
//! Before the fix, an over-full leaf was split by a single fixed byte-halving
//! that did not guarantee each half individually fit a page. A rolling
//! fragment-cache workload (insert new blobs, delete old, occasionally update)
//! with a near-page-sized cell dominating the byte total produced a half that
//! exceeded the page, which either panicked with `attempt to subtract with
//! overflow` inside `serialize_leaf`, or silently wrote a page whose cell
//! pointers pointed past the page end (`cell offset past end of page` on read).
//!
//! These tests drive deterministic churn and assert the file stays valid under
//! graphitesql's own `integrity_check`, under sqlite3's `quick_check` (when
//! available), and that no row is ever lost, duplicated, or corrupted.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::collections::BTreeMap;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn graphite_check(c: &Connection) -> String {
    match &c.query("PRAGMA integrity_check").unwrap().rows[0][0] {
        Value::Text(s) => String::from(s.as_str()),
        _ => "?".into(),
    }
}

/// Tiny deterministic PRNG (SplitMix64) so the workload is reproducible without a
/// dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn pick(&mut self, choices: &[usize]) -> usize {
        choices[(self.next() as usize) % choices.len()]
    }
}

/// A deterministic value of `len` bytes whose content is derived from `tag`, so
/// the exact stored bytes can be verified after the workload. Contains no single
/// quote, so it embeds directly in an SQL string literal.
fn value(tag: u64, len: usize) -> String {
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut s = String::with_capacity(len);
    let mut x = tag | 1;
    for _ in 0..len {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.push(alphabet[(x >> 33) as usize % alphabet.len()] as char);
    }
    s
}

/// Run a rolling insert/delete/update workload with the given size mix and
/// window, verifying validity and exact row preservation throughout.
fn churn(name: &str, seed: u64, iters: usize, window: usize, sizes: &[usize], update: bool) {
    let dir = std::env::temp_dir();
    let path = dir
        .join(format!("gsql-churn-{name}-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);

    let mut expected: BTreeMap<i64, String> = BTreeMap::new();
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE frag(k INTEGER PRIMARY KEY, v BLOB)")
            .unwrap();

        let mut rng = Rng(seed);
        let mut live: std::collections::VecDeque<i64> = Default::default();
        let mut tag: u64 = 0;
        for (it, nextk) in (1_i64..=iters as i64).enumerate() {
            let sz = rng.pick(sizes);
            tag += 1;
            let v = value(tag, sz);
            c.execute(&format!("INSERT INTO frag(k,v) VALUES({nextk},'{v}')"))
                .unwrap();
            expected.insert(nextk, v);
            live.push_back(nextk);

            while live.len() > window {
                let old = live.pop_front().unwrap();
                c.execute(&format!("DELETE FROM frag WHERE k={old}"))
                    .unwrap();
                expected.remove(&old);
            }

            if update && !live.is_empty() && it % 3 == 0 {
                let idx = (rng.next() as usize) % live.len();
                let t = live[idx];
                let sz = rng.pick(sizes);
                tag += 1;
                let v = value(tag, sz);
                c.execute(&format!("UPDATE frag SET v='{v}' WHERE k={t}"))
                    .unwrap();
                expected.insert(t, v);
            }

            // Spot-check integrity periodically (not every op — that is O(n²)).
            if it % 500 == 499 {
                assert_eq!(graphite_check(&c), "ok", "{name}: integrity at it {it}");
            }
        }

        assert_eq!(graphite_check(&c), "ok", "{name}: final integrity");

        // Exact row-set preservation: every key present with its exact bytes,
        // nothing lost or duplicated.
        let res = c.query("SELECT k, v FROM frag ORDER BY k").unwrap();
        assert_eq!(res.rows.len(), expected.len(), "{name}: row count");
        for (row, (k, v)) in res.rows.iter().zip(expected.iter()) {
            let gk = match &row[0] {
                Value::Integer(i) => *i,
                other => panic!("{name}: k not integer: {other:?}"),
            };
            let gv = match &row[1] {
                Value::Text(s) => String::from(s.as_str()),
                Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
                other => panic!("{name}: v unexpected: {other:?}"),
            };
            assert_eq!(gk, *k, "{name}: key mismatch");
            assert_eq!(&gv, v, "{name}: value mismatch for k={k}");
        }
    }

    // sqlite3 must also accept the file graphite wrote.
    if sqlite3_available() {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA quick_check;")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "ok",
            "{name}: sqlite3 quick_check"
        );
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn rolling_fragment_cache_mixed_sizes() {
    churn(
        "mixed",
        7,
        3000,
        60,
        &[12, 40, 120, 400, 900, 1800, 30],
        true,
    );
}

#[test]
fn rolling_fragment_cache_small_window() {
    churn("smallwin", 2, 2500, 12, &[30, 200, 1500, 3000], true);
}

#[test]
fn rolling_fragment_cache_wide_rows() {
    // Rows near half a page (two per leaf) plus in-place growth to near a full
    // page: the exact skew (one large cell dominating the byte total) that broke
    // the old single-halving split.
    churn("wide", 5, 1500, 20, &[1900, 2000, 2040, 1800, 60], true);
}

#[test]
fn insert_only_large_blobs() {
    churn(
        "insertlarge",
        3,
        1500,
        usize::MAX,
        &[900, 1800, 2040, 60],
        false,
    );
}

/// A leaf deliberately grown past 2×page worth of cells in one shot: fill a leaf
/// with several near-half-page rows, then update one to near a full page so the
/// containing leaf must split into more than two parts.
#[test]
fn leaf_exceeds_two_pages_after_growth() {
    let path = std::env::temp_dir()
        .join(format!("gsql-churn-grow-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    let mut expected: BTreeMap<i64, String> = BTreeMap::new();
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(k INTEGER PRIMARY KEY, v BLOB)")
            .unwrap();
        // ~1020-byte rows: four fill a 4KB leaf near-full.
        for k in 1..=40i64 {
            let v = value(k as u64, 1020);
            c.execute(&format!("INSERT INTO t VALUES({k},'{v}')"))
                .unwrap();
            expected.insert(k, v);
        }
        // Grow every third row to ~2000 bytes, then oscillate sizes: repeatedly
        // pushes leaves well past a single page, dominated by one big cell.
        for round in 0..4 {
            for k in (1..=40i64).step_by(if round % 2 == 0 { 3 } else { 2 }) {
                let sz = if round % 2 == 0 { 2040 } else { 8 };
                let v = value((k as u64) << 8 | round as u64, sz);
                c.execute(&format!("UPDATE t SET v='{v}' WHERE k={k}"))
                    .unwrap();
                expected.insert(k, v);
            }
            assert_eq!(graphite_check(&c), "ok", "grow: integrity round {round}");
        }
        let res = c.query("SELECT k, v FROM t ORDER BY k").unwrap();
        assert_eq!(res.rows.len(), expected.len());
        for (row, (k, v)) in res.rows.iter().zip(expected.iter()) {
            let gv = match &row[1] {
                Value::Text(s) => String::from(s.as_str()),
                Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
                other => panic!("v unexpected: {other:?}"),
            };
            assert_eq!(&gv, v, "value mismatch for k={k}");
        }
    }
    if sqlite3_available() {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA quick_check;")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
    }
    let _ = std::fs::remove_file(&path);
}
