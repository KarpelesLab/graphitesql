//! Regression: a ROLLBACK must restore the pager's committed database header.
//!
//! The write-side pager mutates its in-memory `header` in place throughout a
//! transaction — the freelist trunk/count grow and shrink as pages are allocated
//! and freed, `largest_root_page` moves, etc. `rollback()` cleared the overlay
//! and reset `page_count`, but left `header` carrying the rolled-back
//! transaction's freelist state. After a transaction that *grew* the file (adding
//! a freelist trunk page past the committed end) was rolled back, the very next
//! allocation followed that stale trunk pointer and tried to read a page that
//! only ever existed in the discarded, larger file — failing spuriously with
//! `Corrupt("page N out of range")` even though the on-disk database was valid.
//!
//! These tests drive a deterministic multi-transaction commit/rollback workload
//! with large rows (the large blobs are what push the file to grow a freelist
//! trunk during the doomed transaction) and assert: no spurious error, the file
//! stays valid under graphitesql's `integrity_check` and sqlite3's
//! `quick_check`, committed rows persist, rolled-back rows are absent, and the
//! final row-set matches a sqlite3 run of the identical script.

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

/// Tiny deterministic PRNG (SplitMix64) so the workload is reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() as usize) % (hi - lo + 1)
    }
    fn pick(&mut self, choices: &[usize]) -> usize {
        choices[(self.next() as usize) % choices.len()]
    }
}

/// Build the deterministic SQL script and, in lockstep, the expected surviving
/// row set (`id -> body` bytes). A transaction that ends in ROLLBACK contributes
/// none of its inserts/deletes to the expectation. `wal` prepends the WAL pragma.
///
/// Values are fully deterministic (no `randomblob`) so a sqlite3 run of the exact
/// same script must produce an identical row set.
fn build(seed: u64, iters: usize, wal: bool) -> (String, BTreeMap<i64, String>) {
    let mut rng = Rng(seed);
    let mut sql = String::new();
    if wal {
        sql.push_str("PRAGMA journal_mode=WAL;\n");
    }
    sql.push_str("CREATE TABLE docs(id INTEGER PRIMARY KEY, t TEXT, b TEXT, tag TEXT);\n");

    let mut committed: BTreeMap<i64, String> = BTreeMap::new();
    let mut pending: BTreeMap<i64, String> = BTreeMap::new();
    let mut live: std::collections::VecDeque<i64> = Default::default();
    // `live` spans transactions, so snapshot it at BEGIN to restore on ROLLBACK.
    let mut live_at_begin = live.clone();
    let mut nk: i64 = 1;
    let mut open = false;
    let tags = ['a', 'b', 'c', 'd', 'e'];

    for it in 0..iters {
        if it % 20 == 0 {
            sql.push_str("BEGIN;\n");
            pending = committed.clone();
            live_at_begin = live.clone();
            open = true;
        }
        for _ in 0..rng.range(1, 3) {
            // A body large enough to spill into overflow pages / grow the file.
            let blen = rng.pick(&[10, 100, 800]);
            let body = format!("body {nk} {}", "z".repeat(blen));
            let tag = tags[rng.range(0, 4)];
            let title = format!("title {nk}");
            sql.push_str(&format!(
                "INSERT INTO docs VALUES({nk},'{title}','{body}','{tag}');\n"
            ));
            pending.insert(nk, body);
            live.push_back(nk);
            nk += 1;
        }
        if live.len() > 40 {
            for _ in 0..rng.range(1, 5) {
                if let Some(old) = live.pop_front() {
                    sql.push_str(&format!("DELETE FROM docs WHERE id={old};\n"));
                    pending.remove(&old);
                }
            }
        }
        if it % 20 == 19 {
            // Alternate COMMIT / ROLLBACK so the state after a rolled-back,
            // file-growing transaction is exercised as the next txn's start.
            if it % 40 == 39 {
                sql.push_str("ROLLBACK;\n");
                // rolled back: committed map unchanged; restore `live` to its
                // BEGIN snapshot (undo this txn's inserts and deletes).
                live = live_at_begin.clone();
            } else {
                sql.push_str("COMMIT;\n");
                committed = pending.clone();
            }
            open = false;
        }
    }
    // Never leave a dangling open transaction: the Connection-API drop behavior
    // for an open transaction is orthogonal to this test, so close it explicitly.
    if open {
        sql.push_str("COMMIT;\n");
        committed = pending.clone();
    }
    (sql, committed)
}

fn scenario(name: &str, seed: u64, iters: usize, wal: bool) {
    let dir = std::env::temp_dir();
    let gpath = dir
        .join(format!("gsql-rbchurn-{name}-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&gpath);
    let _ = std::fs::remove_file(format!("{gpath}-wal"));

    let (sql, expected) = build(seed, iters, wal);

    {
        let mut c = Connection::create(&gpath).unwrap();
        for stmt in sql.split_inclusive(';') {
            let s = stmt.trim();
            if s.is_empty() {
                continue;
            }
            // The whole point: no statement may fail spuriously.
            c.execute(s)
                .unwrap_or_else(|e| panic!("{name}: statement `{s}` failed: {e}"));
        }
        assert_eq!(graphite_check(&c), "ok", "{name}: graphite integrity_check");

        let res = c.query("SELECT id, b FROM docs ORDER BY id").unwrap();
        let db_ids: std::collections::BTreeSet<i64> = res
            .rows
            .iter()
            .map(|r| match &r[0] {
                Value::Integer(i) => *i,
                _ => -1,
            })
            .collect();
        let exp_ids: std::collections::BTreeSet<i64> = expected.keys().copied().collect();
        let only_db: Vec<_> = db_ids.difference(&exp_ids).collect();
        let only_exp: Vec<_> = exp_ids.difference(&db_ids).collect();
        assert_eq!(
            res.rows.len(),
            expected.len(),
            "{name}: surviving row count (in DB not expected: {only_db:?}; in expected not DB: {only_exp:?})"
        );
        for (row, (id, body)) in res.rows.iter().zip(expected.iter()) {
            let gid = match &row[0] {
                Value::Integer(i) => *i,
                other => panic!("{name}: id not integer: {other:?}"),
            };
            let gb = match &row[1] {
                Value::Text(s) => String::from(s.as_str()),
                other => panic!("{name}: b unexpected: {other:?}"),
            };
            assert_eq!(gid, *id, "{name}: id mismatch");
            assert_eq!(&gb, body, "{name}: body mismatch for id={id}");
        }
    }

    // The file graphite wrote must satisfy sqlite3, and a sqlite3 run of the
    // identical script must yield the identical surviving row set.
    if sqlite3_available() {
        let qc = Command::new("sqlite3")
            .arg(&gpath)
            .arg("PRAGMA quick_check;")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&qc.stdout).trim(),
            "ok",
            "{name}: sqlite3 quick_check"
        );

        let spath = dir
            .join(format!("sqlite-rbchurn-{name}-{}.db", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_file(&spath);
        let _ = std::fs::remove_file(format!("{spath}-wal"));
        let load = Command::new("sqlite3")
            .arg(&spath)
            .arg(&sql)
            .output()
            .unwrap();
        assert!(
            load.status.success(),
            "{name}: sqlite3 load failed: {}",
            String::from_utf8_lossy(&load.stderr)
        );
        let g = Command::new("sqlite3")
            .arg(&gpath)
            .arg("SELECT id, b FROM docs ORDER BY id;")
            .output()
            .unwrap();
        let s = Command::new("sqlite3")
            .arg(&spath)
            .arg("SELECT id, b FROM docs ORDER BY id;")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&g.stdout),
            String::from_utf8_lossy(&s.stdout),
            "{name}: row set differs from sqlite3"
        );
        let _ = std::fs::remove_file(&spath);
        let _ = std::fs::remove_file(format!("{spath}-wal"));
    }

    let _ = std::fs::remove_file(&gpath);
    let _ = std::fs::remove_file(format!("{gpath}-wal"));
}

#[test]
fn rollback_after_growth_then_insert_seed11() {
    // The originally-reported reproduction shape (large blobs, ~42 iterations).
    scenario("seed11", 11, 42, false);
}

#[test]
fn rollback_churn_multiple_seeds() {
    for seed in [1, 7, 23, 99] {
        scenario(&format!("s{seed}"), seed, 60, false);
    }
}

#[test]
fn rollback_churn_wal_mode() {
    for seed in [11, 23] {
        scenario(&format!("wal{seed}"), seed, 60, true);
    }
}

/// Directly assert the invariant end-to-end: a transaction that grows the file
/// and is rolled back leaves no trace, and the next transaction commits cleanly.
#[test]
fn rolled_back_growth_leaves_no_trace() {
    let dir = std::env::temp_dir();
    let path = dir
        .join(format!("gsql-rbtrace-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);

    let big = "q".repeat(4000); // forces overflow pages / file growth
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, b TEXT)")
            .unwrap();
        // Commit a baseline that grows and frees pages (leaves a freelist).
        c.execute("BEGIN").unwrap();
        for i in 1..=30 {
            c.execute(&format!("INSERT INTO t VALUES({i},'{big}')"))
                .unwrap();
        }
        for i in 1..=20 {
            c.execute(&format!("DELETE FROM t WHERE id={i}")).unwrap();
        }
        c.execute("COMMIT").unwrap();

        // A transaction that grows the file further, then rolls back.
        c.execute("BEGIN").unwrap();
        for i in 100..=140 {
            c.execute(&format!("INSERT INTO t VALUES({i},'{big}')"))
                .unwrap();
        }
        c.execute("ROLLBACK").unwrap();

        // The next transaction must not trip over a stale freelist pointer.
        c.execute("BEGIN").unwrap();
        for i in 200..=210 {
            c.execute(&format!("INSERT INTO t VALUES({i},'{big}')"))
                .unwrap();
        }
        c.execute("COMMIT").unwrap();

        assert_eq!(graphite_check(&c), "ok", "integrity after rollback+commit");

        // Rolled-back ids absent; committed ids present.
        let rolled = c
            .query("SELECT count(*) FROM t WHERE id BETWEEN 100 AND 140")
            .unwrap();
        assert_eq!(
            rolled.rows[0][0],
            Value::Integer(0),
            "rolled-back rows leaked"
        );
        let kept = c.query("SELECT count(*) FROM t WHERE id >= 200").unwrap();
        assert_eq!(kept.rows[0][0], Value::Integer(11), "committed rows lost");
        let base = c
            .query("SELECT count(*) FROM t WHERE id BETWEEN 21 AND 30")
            .unwrap();
        assert_eq!(base.rows[0][0], Value::Integer(10), "baseline rows lost");
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
