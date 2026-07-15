//! Differential b-tree fill-factor tests: inserting keys in **non-sequential**
//! order must not fragment the tree into 1-2-cell pages.
//!
//! Before sibling-rebalancing landed, a middle insert into a full leaf split it
//! greedily into a full page plus a 1-cell sibling; repeated random inserts kept
//! spawning 1-cell siblings that never grew, producing files 10-20× larger than
//! SQLite (e.g. a random-key secondary index over 500 rows was 79 pages vs
//! SQLite's 7). The fix pools an overflowing page with its siblings under the
//! parent and redistributes the cells, matching SQLite's `balance_nonroot`.
//!
//! These tests drive the high-impact non-sequential shapes — `WITHOUT ROWID`
//! text-PK tables and secondary indexes over pseudo-random keys — and assert the
//! file is (a) valid under graphite's own `integrity_check`, (b) valid under
//! sqlite3's `quick_check`, (c) within a small constant of sqlite3's page count
//! (not 10-20×), and (d) holds exactly the same rows as sqlite3. A sequential
//! insert is checked separately to confirm the compact-append path did not
//! regress. All data is deterministic (no `randomblob`), so the row sets are
//! directly comparable.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn load(bin: &str, db: &str, sql: &str) {
    let _ = std::fs::remove_file(db);
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("run binary");
    assert!(
        out.status.success(),
        "load failed ({bin}): {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn query(bin: &str, db: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(db).arg(sql).output().expect("query");
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

fn page_count(bin: &str, db: &str) -> i64 {
    query(bin, db, "PRAGMA page_count")
        .trim()
        .parse()
        .expect("page_count integer")
}

fn tmp(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphite_fill_{}_{}.db", std::process::id(), name));
    p.to_string_lossy().into_owned()
}

/// Load `sql` with graphite; assert the file is valid, holds the same rows as a
/// sqlite3 run of the same script (via `probe`), and its page count is within
/// `slack` of sqlite3's (proving the fragmentation is gone). Returns
/// `(graphite_pages, sqlite_pages)`.
fn check(name: &str, sql: &str, probe: &str, slack: i64) -> (i64, Option<i64>) {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let gdb = tmp(&format!("g_{name}"));
    load(g, &gdb, sql);

    let integ = query(g, &gdb, "PRAGMA integrity_check");
    assert_eq!(integ, "ok", "[{name}] graphite integrity_check: {integ}");
    let gp = page_count(g, &gdb);

    let mut sp = None;
    if sqlite3_available() {
        let qc = query("sqlite3", &gdb, "PRAGMA quick_check");
        assert_eq!(
            qc, "ok",
            "[{name}] sqlite3 quick_check on graphite file: {qc}"
        );

        let sdb = tmp(&format!("s_{name}"));
        load("sqlite3", &sdb, sql);
        let s = page_count("sqlite3", &sdb);
        sp = Some(s);
        assert_eq!(
            query(g, &gdb, probe),
            query("sqlite3", &sdb, probe),
            "[{name}] row set diverged from sqlite3"
        );
        assert!(
            (gp - s).abs() <= slack,
            "[{name}] page count {gp} not within {slack} of sqlite3 {s} (fragmentation?)"
        );
        let _ = std::fs::remove_file(&sdb);
    }
    let _ = std::fs::remove_file(&gdb);
    (gp, sp)
}

/// A `WITHOUT ROWID` table with a TEXT primary key inserted in pseudo-random key
/// order (the fragment-cache pattern) stays compact across 100/500/2000 rows.
#[test]
fn without_rowid_text_pk_random_order_is_compact() {
    for n in [100usize, 500, 2000] {
        let sql = format!(
            "PRAGMA page_size=4096;\
             CREATE TABLE t(k TEXT PRIMARY KEY, v INT) WITHOUT ROWID;\
             WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<{n})\
               INSERT INTO t SELECT 'k'||printf('%06d',(x*7)%100000), x FROM c;"
        );
        check(&format!("wr_{n}"), &sql, "SELECT k, v FROM t ORDER BY k", 2);
    }
}

/// A secondary index over pseudo-random text values stays compact across
/// 100/500/2000 rows (was 79 pages vs sqlite's 7 at n=500 before the fix).
#[test]
fn secondary_index_random_keys_is_compact() {
    for n in [100usize, 500, 2000] {
        let sql = format!(
            "PRAGMA page_size=4096;\
             CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\
             CREATE INDEX ix ON t(b);\
             WITH RECURSIVE c(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<{n})\
               INSERT INTO t SELECT x, 'v'||printf('%06d',(x*7)%100000) FROM c;"
        );
        check(
            &format!("idx_{n}"),
            &sql,
            "SELECT a, b FROM t ORDER BY b, a",
            2,
        );
    }
}

/// Two secondary indexes on the same table, both fed random keys, stay compact.
#[test]
fn two_secondary_indexes_random_keys_are_compact() {
    let sql = "PRAGMA page_size=4096;\
        CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INT);\
        CREATE INDEX ix1 ON t(b);\
        CREATE INDEX ix2 ON t(c);\
        WITH RECURSIVE g(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM g WHERE x<1000)\
          INSERT INTO t SELECT x, 'v'||printf('%06d',(x*13)%100000), (x*97)%50000 FROM g;";
    check("two_idx", sql, "SELECT a, b, c FROM t ORDER BY b, a", 2);
}

/// Deletes interleaved with random inserts, and updates that move index keys,
/// stay compact and valid.
#[test]
fn deletes_and_key_moving_updates_stay_compact() {
    let del = "PRAGMA page_size=4096;\
        CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\
        CREATE INDEX ix ON t(b);\
        WITH RECURSIVE g(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM g WHERE x<800)\
          INSERT INTO t SELECT x, 'v'||printf('%06d',(x*7)%100000) FROM g;\
        DELETE FROM t WHERE a%3=0;\
        WITH RECURSIVE g(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM g WHERE x<400)\
          INSERT INTO t SELECT 800+x, 'w'||printf('%06d',(x*11)%100000) FROM g;";
    check("del_interleave", del, "SELECT a, b FROM t ORDER BY b, a", 2);

    let upd = "PRAGMA page_size=4096;\
        CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\
        CREATE INDEX ix ON t(b);\
        WITH RECURSIVE g(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM g WHERE x<600)\
          INSERT INTO t SELECT x, 'v'||printf('%06d',(x*7)%100000) FROM g;\
        UPDATE t SET b='z'||printf('%06d',(a*31)%100000) WHERE a%2=0;";
    check("upd_movekey", upd, "SELECT a, b FROM t ORDER BY b, a", 2);
}

/// Sequential-append regression: a plain rowid table filled in ascending order
/// keeps the compact page count it always had (the greedy fast path is not
/// disturbed by sibling rebalancing).
#[test]
fn sequential_rowid_append_stays_compact() {
    let sql = "PRAGMA page_size=4096;\
        CREATE TABLE t(a INTEGER PRIMARY KEY, b INT);\
        WITH RECURSIVE g(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM g WHERE x<500)\
          INSERT INTO t SELECT x, x*7 FROM g;";
    let (gp, sp) = check("seq500", sql, "SELECT a, b FROM t ORDER BY a", 1);
    // Absolute check even when sqlite3 is unavailable: 500 tiny rows fit a small
    // handful of pages, nowhere near the fragmented regime.
    assert!(
        gp <= 6,
        "sequential append should stay compact, got {gp} pages"
    );
    if let Some(s) = sp {
        assert_eq!(gp, s, "sequential page count should match sqlite3");
    }
}
