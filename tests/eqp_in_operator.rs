//! `[NOT] IN (SELECT …)` over an *indexed* subquery column, evaluated by
//! iterating that index rather than materializing the result, renders SQLite's
//! `… FOR IN-OPERATOR` plan node in place of the `LIST SUBQUERY` / `CREATE BLOOM
//! FILTER` subtree. This fires only when the outer table is scanned (its IN
//! column is not itself index-seekable) and the subquery is a *simple*
//! `SELECT <col> FROM <table> [ORDER BY …]` whose single plain column is
//! indexed: a secondary index leading with the column → `USING INDEX <name> FOR
//! IN-OPERATOR`; the rowid / INTEGER PRIMARY KEY → `USING ROWID SEARCH ON TABLE
//! <table> FOR IN-OPERATOR`. Any WHERE/LIMIT/DISTINCT/GROUP/join/expression, an
//! unindexed column, or an ambiguous multi-index choice keeps the `LIST
//! SUBQUERY` form. EQP-only: the executed results are unaffected. Verified
//! byte-for-byte against the sqlite3 3.50.4 CLI.

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

// `w` is the scanned outer (no index on `p`); `t` supplies the subquery values
// with exactly one index per candidate column (unambiguous choice).
const BASE: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c);\
     CREATE INDEX tb ON t(b);\
     INSERT INTO t VALUES(1,10,100),(2,20,200),(3,30,300);\
     CREATE TABLE u(x, y); CREATE INDEX ux ON u(x); INSERT INTO u VALUES(10,1),(30,3);\
     CREATE TABLE w(p, q); INSERT INTO w VALUES(10,1),(20,2),(99,3);";

#[test]
fn for_in_operator_plan_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        // Secondary-index column → USING INDEX tb FOR IN-OPERATOR.
        "SELECT * FROM w WHERE p IN (SELECT b FROM t)",
        "SELECT * FROM w WHERE p NOT IN (SELECT b FROM t)",
        "SELECT * FROM w WHERE p IN (SELECT b FROM t ORDER BY b)",
        "SELECT * FROM w WHERE p IN (SELECT (b) FROM t)",
        "SELECT * FROM w WHERE p IN (SELECT b AS bb FROM t)",
        "SELECT * FROM w WHERE p IN (SELECT t.b FROM t)",
        // rowid / INTEGER PRIMARY KEY → USING ROWID SEARCH ON TABLE t.
        "SELECT * FROM w WHERE p IN (SELECT a FROM t)",
        "SELECT * FROM w WHERE p IN (SELECT rowid FROM t)",
        "SELECT * FROM w WHERE p NOT IN (SELECT a FROM t)",
        // Controls that keep the LIST SUBQUERY / bloom form:
        "SELECT * FROM w WHERE p IN (SELECT c FROM t)", // unindexed column
        "SELECT * FROM w WHERE p IN (SELECT b FROM t WHERE c > 0)", // WHERE
        "SELECT * FROM w WHERE p IN (SELECT b FROM t LIMIT 2)", // LIMIT
        "SELECT * FROM w WHERE p IN (SELECT DISTINCT b FROM t)", // DISTINCT
        "SELECT * FROM w WHERE p IN (SELECT b + 1 FROM t)", // expression
        // Control: the IN column is itself index-seekable → outer SEARCH, RHS
        // stays a materialized LIST SUBQUERY (no FOR IN-OPERATOR).
        "SELECT * FROM u WHERE x IN (SELECT b FROM t)",
        "SELECT * FROM t WHERE b IN (SELECT x FROM u)",
    ] {
        assert_eq!(plan("sqlite3", BASE, q), plan(g, BASE, q), "plan for {q}");
    }
}

/// The plan node is cosmetic — the executed rows must be identical regardless.
#[test]
fn for_in_operator_rows_match() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT p FROM w WHERE p IN (SELECT b FROM t) ORDER BY p",
        "SELECT p FROM w WHERE p NOT IN (SELECT b FROM t) ORDER BY p",
        "SELECT p FROM w WHERE p IN (SELECT a FROM t) ORDER BY p",
    ] {
        let full = format!("{BASE} {q}");
        let sq = Command::new("sqlite3")
            .arg(":memory:")
            .arg(&full)
            .output()
            .unwrap();
        let gr = Command::new(g).arg(":memory:").arg(&full).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&sq.stdout),
            String::from_utf8_lossy(&gr.stdout),
            "rows for {q}"
        );
    }
}
