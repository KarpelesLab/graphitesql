//! Secondary-index SEEK choice for an equality/`IS NULL`-prefix `WHERE`. Among
//! the plain indexes that can seek the constrained leading prefix, SQLite 3.50.4:
//!
//!   1. prefers a **query-covering** index (holds every referenced column, or the
//!      rowid, so the table b-tree lookup is skipped) over a non-covering one,
//!      even if the covering index is wider;
//!   2. among covering candidates, picks the **narrower** estimated key width
//!      (its `estimateIndexWidth`/`LogEst` cost), ties → the **newest** index
//!      (highest rootpage);
//!   3. among non-covering candidates picks by `sqlite_stat1` selectivity when
//!      statistics exist, ties → the **newest** index.
//!
//! graphite's chooser (`choose_seek_index`, shared by the executor's seek and the
//! `EXPLAIN QUERY PLAN` render) mirrors this. Verified byte-for-byte — plan AND
//! rows, since the chosen index changes row order — against the sqlite3 3.50.4
//! CLI. Skips when the CLI is absent.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn plan(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} EXPLAIN QUERY PLAN {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|c: char| " |`*+_-".contains(c)))
        .collect::<Vec<_>>()
        .join("#")
}

fn rows(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn seek_index_choice_plan_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Covering vs non-covering (rule 1): `ia`=(a) does not cover `SELECT *` /
    // `SELECT b` (needs b), but `iab`=(a,b) does — so the wider covering index wins.
    let cov = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);\
               CREATE INDEX ia ON t(a);CREATE INDEX iab ON t(a,b);\
               INSERT INTO t VALUES(1,1,10),(2,1,20),(3,2,30);";
    // Same schema created in the OPPOSITE order — covering still wins (not a
    // creation-order artifact).
    let cov_rev = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);\
                   CREATE INDEX iab ON t(a,b);CREATE INDEX ia ON t(a);\
                   INSERT INTO t VALUES(1,1,10),(2,1,20),(3,2,30);";
    // Both cover `count(*)` (rule 2): the NARROWER `ia` beats `iab` (regardless of
    // creation order).
    let both_cover = cov;
    let both_cover_rev = cov_rev;
    // Two same-width covering indexes on (a): tie → NEWEST (`ia2`).
    let cov_tie = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);\
                   CREATE INDEX ia ON t(a);CREATE INDEX ia2 ON t(a);\
                   INSERT INTO t VALUES(1,1,10),(2,1,20),(3,2,30);";
    // Non-covering, equal 1-col prefix, no stats (rule 3): tie → NEWEST. `ia`+`ib`
    // both match a single column of `a=? AND b=?`; the newest wins either order.
    let noncov = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);\
                  CREATE INDEX ia ON t(a);CREATE INDEX ib ON t(b);\
                  INSERT INTO t VALUES(1,1,10),(2,2,10),(3,1,20);";
    let noncov_rev = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);\
                      CREATE INDEX ib ON t(b);CREATE INDEX ia ON t(a);\
                      INSERT INTO t VALUES(1,1,10),(2,2,10),(3,1,20);";
    // Longer matched prefix (more selective) still wins over a shorter one, even
    // when neither covers `SELECT *`.
    let longer = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b,c);\
                  CREATE INDEX ia ON t(a);CREATE INDEX iab ON t(a,b);\
                  INSERT INTO t VALUES(1,1,2,3),(2,1,3,4);";
    // IS NULL-prefix seek is covered by `iab` too.
    let isnull = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);\
                  CREATE INDEX ia ON t(a);CREATE INDEX iab ON t(a,b);\
                  INSERT INTO t VALUES(1,NULL,10),(2,1,20);";
    let cases: &[(&str, &str)] = &[
        (cov, "SELECT * FROM t WHERE a=1"),
        (cov, "SELECT b FROM t WHERE a=1"),
        (cov_rev, "SELECT * FROM t WHERE a=1"),
        (both_cover, "SELECT count(*) FROM t WHERE a=1"),
        (both_cover_rev, "SELECT count(*) FROM t WHERE a=1"),
        (cov_tie, "SELECT count(*) FROM t WHERE a=1"),
        (cov_tie, "SELECT * FROM t WHERE a=1"),
        (noncov, "SELECT * FROM t WHERE a=1 AND b=10"),
        (noncov_rev, "SELECT * FROM t WHERE a=1 AND b=10"),
        (longer, "SELECT * FROM t WHERE a=1 AND b=2"),
        (isnull, "SELECT * FROM t WHERE a IS NULL"),
    ];
    for (base, q) in cases {
        assert_eq!(
            plan("sqlite3", base, q),
            plan(g, base, q),
            "plan for `{q}` :: {base}"
        );
    }
}

#[test]
fn seek_index_choice_rows_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // The chosen index changes row order; rows must still match SQLite with no
    // ORDER BY.
    let cov = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);\
               CREATE INDEX ia ON t(a);CREATE INDEX iab ON t(a,b);\
               INSERT INTO t VALUES(1,1,30),(2,1,10),(3,1,20),(4,2,5);";
    let noncov = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);\
                  CREATE INDEX ia ON t(a);CREATE INDEX ib ON t(b);\
                  INSERT INTO t VALUES(1,1,10),(2,2,10),(3,1,10),(4,1,20);";
    let isnull = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);\
                  CREATE INDEX ia ON t(a);CREATE INDEX iab ON t(a,b);\
                  INSERT INTO t VALUES(1,NULL,10),(2,NULL,5),(3,1,20);";
    let cases: &[(&str, &str)] = &[
        (cov, "SELECT * FROM t WHERE a=1"),
        (cov, "SELECT b FROM t WHERE a=1"),
        (cov, "SELECT count(*) FROM t WHERE a=1"),
        (noncov, "SELECT * FROM t WHERE a=1 AND b=10"),
        (isnull, "SELECT * FROM t WHERE a IS NULL"),
    ];
    for (base, q) in cases {
        assert_eq!(
            rows("sqlite3", base, q),
            rows(g, base, q),
            "rows for `{q}` :: {base}"
        );
    }
}

/// A `sqlite_stat1`/ANALYZE case: with statistics the selective non-covering
/// index wins over the non-selective one — selectivity dominates the newest
/// tiebreak. Verified against the CLI (which runs ANALYZE the same way).
#[test]
fn seek_index_choice_stat1_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // `a` has 2 distinct values, `b` has ~100 — `ib` is far more selective. Create
    // `ib` FIRST so the newest tiebreak would otherwise pick `ia`; ANALYZE must
    // override that.
    let base = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);\
                CREATE INDEX ib ON t(b);CREATE INDEX ia ON t(a);\
                INSERT INTO t(a,b) SELECT value%2, value%100 FROM generate_series(1,1000);\
                ANALYZE;";
    let q = "SELECT * FROM t WHERE a=1 AND b=5";
    assert_eq!(plan("sqlite3", base, q), plan(g, base, q), "stat1 plan");
    assert_eq!(rows("sqlite3", base, q), rows(g, base, q), "stat1 rows");
}
