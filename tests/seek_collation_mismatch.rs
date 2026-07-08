//! An equality whose explicit `COLLATE` differs from the column's collation cannot
//! be served by that column's index — the index orders keys for a *different*
//! collation — so SQLite scans (e.g. `b = 'x' COLLATE NOCASE` over a BINARY index on
//! `b`). graphite used to treat it as a plain equality seek, which both diverged in
//! `EXPLAIN QUERY PLAN` (`SEARCH` vs `SCAN`) and, worse, *mis-ordered* rows: it
//! credited the seek's `(b, rowid)` walk for a sole `ORDER BY` on the rowid, so the
//! rows came back in index order instead of rowid order.
//!
//! The fix is one collation check in the shared `collect_eq_constraints`, so the
//! executor seek, the EQP label, and the ORDER-BY credit all move in lockstep.
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

const BINARY_COL: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c); CREATE INDEX tb ON t(b); \
                          INSERT INTO t VALUES(1,'x',5),(2,'X',6),(3,'y',7),(4,'XX',8);";
const NOCASE_COL: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT COLLATE NOCASE, c); CREATE INDEX tb ON t(b); \
     INSERT INTO t VALUES(1,'x',5),(2,'X',6),(3,'y',7);";

#[test]
fn collation_mismatch_scans_matching_seeks() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // BINARY column / index: an explicit NOCASE differs → SCAN; BINARY (or absent) → SEARCH.
    // Equality and range alike; a BETWEEN keeps only the bound whose collation matches.
    for q in [
        "SELECT * FROM t WHERE b='x' COLLATE NOCASE",
        "SELECT * FROM t WHERE b='x' COLLATE BINARY",
        "SELECT * FROM t WHERE b='x'",
        "SELECT * FROM t WHERE b>'x' COLLATE NOCASE",
        "SELECT * FROM t WHERE b>'x' COLLATE BINARY",
        "SELECT * FROM t WHERE b>'x'",
        "SELECT * FROM t WHERE b BETWEEN 'a' COLLATE NOCASE AND 'z'",
        "SELECT * FROM t WHERE b BETWEEN 'a' AND 'z'",
    ] {
        assert_eq!(
            plan("sqlite3", BINARY_COL, q),
            plan(g, BINARY_COL, q),
            "plan for {q}"
        );
    }
    // NOCASE column / index: NOCASE (or absent) matches → SEARCH; explicit BINARY differs → SCAN.
    for q in [
        "SELECT * FROM t WHERE b='x'",
        "SELECT * FROM t WHERE b='x' COLLATE NOCASE",
        "SELECT * FROM t WHERE b='x' COLLATE BINARY",
    ] {
        assert_eq!(
            plan("sqlite3", NOCASE_COL, q),
            plan(g, NOCASE_COL, q),
            "plan for {q}"
        );
    }
}

#[test]
fn collation_mismatch_orders_rows_correctly() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // The previously-mis-ordered case: a NOCASE match spans multiple binary keys, so
    // the seek's index order is NOT rowid order — `ORDER BY a` must still sort.
    for (base, q) in [
        (
            BINARY_COL,
            "SELECT a,b FROM t WHERE b='x' COLLATE NOCASE ORDER BY a",
        ),
        (
            BINARY_COL,
            "SELECT a,b FROM t WHERE b='x' COLLATE BINARY ORDER BY a",
        ),
        (BINARY_COL, "SELECT a,b FROM t WHERE b='x' ORDER BY a"),
        (
            BINARY_COL,
            "SELECT a FROM t WHERE b>'x' COLLATE NOCASE ORDER BY a",
        ),
        (
            BINARY_COL,
            "SELECT a FROM t WHERE b BETWEEN 'a' COLLATE NOCASE AND 'z' ORDER BY a",
        ),
        (
            NOCASE_COL,
            "SELECT a,b FROM t WHERE b='x' COLLATE BINARY ORDER BY a",
        ),
        (NOCASE_COL, "SELECT a,b FROM t WHERE b='x' ORDER BY a"),
    ] {
        assert_eq!(rows("sqlite3", base, q), rows(g, base, q), "rows for {q}");
    }
}
