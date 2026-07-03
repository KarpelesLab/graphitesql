//! B9e — a comparison against a *non-correlated scalar subquery* seeks the index,
//! exactly as a constant would. `WHERE b = (SELECT max(x) FROM u)` reads
//! `SEARCH t USING INDEX tb (b=?)` (plus the existing `SCALAR SUBQUERY N` child) in
//! SQLite, which evaluates the subquery once and plans a seek; graphite used to SCAN
//! because the eq-collector required a constant RHS. The executor folds the
//! subquery to its value before the seek; `eqp_access` recognizes the shape
//! *structurally* (without running the subquery, matching SQLite — `EXPLAIN` never
//! evaluates it, so even `b = (SELECT 1/0)` plans a `SEARCH`). Equality and range,
//! secondary index and INTEGER PRIMARY KEY.
//!
//! A *correlated* body, `EXISTS`, `IN (SELECT)`, and a bare-column subquery
//! (`(SELECT x FROM u)` — folding it would drop the column's affinity) do NOT seek —
//! the outer table stays a `SCAN`, and the full `WHERE` is re-applied so rows are
//! exact regardless. Verified vs the sqlite3 3.50.4 CLI.

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

const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); CREATE INDEX tb ON t(b); \
                      CREATE TABLE u(x,y); CREATE INDEX ux ON u(x);";

#[test]
fn scalar_subquery_operand_seeks_like_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM t WHERE b=(SELECT max(x) FROM u)", // secondary index equality
        "SELECT * FROM t WHERE a=(SELECT max(x) FROM u)", // INTEGER PRIMARY KEY
        "SELECT * FROM t WHERE b>(SELECT max(x) FROM u)", // range
        "SELECT * FROM t WHERE b=(SELECT max(x) FROM u) AND c=1",
        "SELECT * FROM t WHERE b=(SELECT 5)", // constant subquery
        "SELECT c FROM t WHERE b=(SELECT max(x) FROM u)", // covering
        "SELECT * FROM t WHERE b=(SELECT 1/0)", // EXPLAIN never runs the subquery
    ] {
        assert_eq!(
            plan("sqlite3", SCHEMA, q),
            plan(g, SCHEMA, q),
            "plan for {q}"
        );
    }
}

#[test]
fn non_seekable_subquery_shapes_stay_scan() {
    // A correlated / EXISTS / IN (SELECT) / bare-column subquery must not seek the
    // outer table — graphite keeps the SCAN (results still correct via the WHERE
    // re-apply). (SQLite renders extra CORRELATED / LIST SUBQUERY nodes graphite does
    // not model, so we assert only that the outer access is not a SEARCH.)
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM t WHERE b=(SELECT y FROM u WHERE x=t.a)", // correlated
        "SELECT * FROM t WHERE b IN (SELECT x FROM u)",          // IN (SELECT)
        "SELECT * FROM t WHERE EXISTS(SELECT 1 FROM u WHERE x=b)", // correlated EXISTS
        "SELECT * FROM t WHERE b=(SELECT x FROM u)",             // bare-column projection
    ] {
        let got = plan(g, SCHEMA, q);
        assert!(
            !got.contains("SEARCH t "),
            "{q} should not seek the outer table t, got {got:?}"
        );
    }
}

#[test]
fn scalar_subquery_seek_rows_match() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = format!(
        "{SCHEMA} INSERT INTO t VALUES(1,10,1),(2,20,2),(3,30,3),(4,20,4); \
         INSERT INTO u VALUES(20,1),(10,2),(5,3);"
    );
    for q in [
        "SELECT a FROM t WHERE b=(SELECT max(x) FROM u) ORDER BY a",
        "SELECT a FROM t WHERE a=(SELECT max(x) FROM u) ORDER BY a",
        "SELECT a FROM t WHERE b>(SELECT min(x) FROM u) ORDER BY a",
        "SELECT count(*) FROM t WHERE b=(SELECT max(x) FROM u)",
        "SELECT a FROM t WHERE b=(SELECT max(x) FROM u WHERE x>999) ORDER BY a", // NULL → no rows
        "SELECT a FROM t WHERE b=(SELECT x FROM u ORDER BY x DESC LIMIT 1) ORDER BY a",
    ] {
        assert_eq!(rows("sqlite3", &base, q), rows(g, &base, q), "rows for {q}");
    }
}
