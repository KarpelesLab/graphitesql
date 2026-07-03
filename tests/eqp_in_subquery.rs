//! B9a — a single non-correlated `[NOT] IN (SELECT …)` in the WHERE renders a
//! `LIST SUBQUERY 1` node (its body plan + a `CREATE BLOOM FILTER` child) after the
//! table access, matching SQLite. graphite used to render just the bare access.
//!
//! Rendered only where the whole plan is provably byte-exact: graphite's access is a
//! bare `SCAN` (so there is no seek to diverge from SQLite's cost-model choice), and
//! either the form is `NOT IN` (which never seeks the IN column) or the IN column is
//! not seekable (so SQLite also scans). A *positive* `IN` on an indexed / rowid column
//! — which SQLite serves with a per-candidate `SEARCH` — declines (that seek is a
//! separate follow-up), as do a correlated / compound / cross-position subquery.
//! Verified vs the sqlite3 3.50.4 CLI.

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
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

// `b` is indexed, `c`/`d` are not; `u`/`w` are the subquery sources.
const BASE: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c, d); CREATE INDEX tb ON t(b); \
                    CREATE TABLE u(x,y); CREATE INDEX ux ON u(x); CREATE TABLE w(z);";

#[test]
fn in_subquery_list_subquery_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        // NOT IN never seeks the IN column → the access stays SCAN in both.
        "SELECT * FROM t WHERE b NOT IN (SELECT y FROM u)",
        "SELECT * FROM t WHERE c NOT IN (SELECT z FROM w)",
        "SELECT * FROM t WHERE b NOT IN (SELECT z FROM w WHERE z>3)",
        "SELECT * FROM t WHERE b NOT IN (SELECT y FROM u) AND c=5",
        // Positive IN on an UNINDEXED column → SQLite also scans.
        "SELECT * FROM t WHERE d IN (SELECT y FROM u)",
        "SELECT d FROM t WHERE d IN (SELECT y FROM u)",
    ] {
        assert_eq!(plan("sqlite3", BASE, q), plan(g, BASE, q), "plan for {q}");
    }
}

#[test]
fn in_subquery_out_of_subset_declines_to_bare_scan() {
    // These render a SEARCH / different node shape in sqlite that graphite doesn't
    // reproduce yet; graphite must keep its prior bare `SCAN t` (no LIST SUBQUERY /
    // bloom node emitted into a non-matching plan).
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM t WHERE b IN (SELECT y FROM u)", // positive IN on indexed col → SQLite SEARCHes
        "SELECT * FROM t WHERE a IN (SELECT x FROM u)", // rowid seek
        "SELECT * FROM t WHERE rowid IN (SELECT x FROM u)",
        "SELECT * FROM t WHERE b IN (SELECT y FROM u WHERE u.y=t.a)", // correlated
        "SELECT * FROM t WHERE b NOT IN (SELECT y FROM u UNION SELECT x FROM u)", // compound body
        "SELECT * FROM t WHERE d IN (SELECT y FROM u) AND (SELECT count(*) FROM w)>0", // cross-position
    ] {
        assert_eq!(plan(g, BASE, q), "SCAN t", "expected bare SCAN for {q}");
    }
}

#[test]
fn in_subquery_rows_unaffected() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = format!(
        "{BASE} INSERT INTO t VALUES(1,10,5,7),(2,20,6,8),(3,30,7,7),(4,40,8,9); \
         INSERT INTO u VALUES(7,7),(8,8);"
    );
    for q in [
        "SELECT a FROM t WHERE b NOT IN (SELECT y FROM u) ORDER BY a",
        "SELECT a FROM t WHERE d IN (SELECT y FROM u) ORDER BY a",
        "SELECT count(*) FROM t WHERE c NOT IN (SELECT x FROM u)",
    ] {
        assert_eq!(rows("sqlite3", &base, q), rows(g, &base, q), "rows for {q}");
    }
}
