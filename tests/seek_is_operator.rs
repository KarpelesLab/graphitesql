//! `col IS <non-null constant>` is seekable. SQLite's `IS` operator behaves
//! exactly like `=` for non-NULL operands (a NULL `col` makes both false), so an
//! index / rowid equality seek on `col IS 5` finds the same rows as `col = 5`.
//! graphite previously treated `IS` as un-seekable and fell back to a full scan, so
//! `EXPLAIN QUERY PLAN` diverged (`SCAN` vs `SEARCH`). The fix lives in the shared
//! `collect_eq_constraints`, so the executor seek and the EQP label move in lockstep.
//!
//! `col IS NULL` stays the dedicated NULL-key seek and `col IS NOT <const>` stays a
//! scan (a `!=`), both unchanged. Verified vs the sqlite3 3.50.4 CLI.

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

const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); CREATE INDEX tb ON t(b);";
const DATA: &str =
    "INSERT INTO t VALUES(1,5,5),(2,5,9),(3,NULL,5),(4,7,5),(5,'x',1),(6,NULL,NULL);";

#[test]
fn is_constant_seeks_like_equality() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM t WHERE b IS 5",   // secondary-index seek
        "SELECT * FROM t WHERE b IS 'x'", // text key
        "SELECT * FROM t WHERE a IS 3",   // INTEGER PRIMARY KEY seek
        "SELECT * FROM t WHERE b IS 5 AND c IS 6",
        "SELECT count(*) FROM t WHERE b IS 5",
        // Unchanged: NULL-key seek and the IS-NOT scan.
        "SELECT * FROM t WHERE b IS NULL",
        "SELECT * FROM t WHERE b IS NOT 5",
    ] {
        assert_eq!(
            plan("sqlite3", SCHEMA, q),
            plan(g, SCHEMA, q),
            "plan for {q}"
        );
    }
}

#[test]
fn is_operator_rows_match_with_nulls() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = format!("{SCHEMA} {DATA}");
    for q in [
        "SELECT a FROM t WHERE b IS 5 ORDER BY a",
        "SELECT a FROM t WHERE b IS NULL ORDER BY a",
        "SELECT a FROM t WHERE b IS 'x' ORDER BY a",
        "SELECT a FROM t WHERE b IS NOT 5 ORDER BY a",
        "SELECT a FROM t WHERE a IS 3 ORDER BY a",
        "SELECT count(*) FROM t WHERE b IS 5",
    ] {
        assert_eq!(rows("sqlite3", &base, q), rows(g, &base, q), "rows for {q}");
    }
}
