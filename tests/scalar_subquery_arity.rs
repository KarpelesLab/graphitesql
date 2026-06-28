//! A multi-column subquery used where a single value is expected is a
//! prepare-time error in SQLite (`sub-select returns N columns - expected 1`),
//! raised before the query runs. graphite resolved the subquery lazily, per row,
//! so it silently accepted the mismatch over an empty (or fully filtered) table
//! where no row ever evaluated the expression; it now reports the same arity
//! error at prepare time, on both the SELECT and the UPDATE/DELETE paths.
//!
//! The check deliberately does NOT fire when the subquery is a direct operand of
//! a comparison operator (`=`, `<`, `IS`, …) or `BETWEEN`: there SQLite reports
//! `row value misused` instead, which is a separate diagnostic left to its
//! existing behaviour and not asserted here.
//!
//! Column resolution still wins: the check fires only when every column the
//! subquery references resolves, so a `no such column` (SQLite's first error) is
//! never masked by an arity report. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// First non-caret line of combined stdout/stderr, error-prefix stripped.
fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().next().unwrap_or("").trim_end().to_string();
    if !line.is_empty() {
        return line;
    }
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .find(|l| !l.trim_start().starts_with('^'))
        .unwrap_or("")
        .trim_start_matches("Error: in prepare, ")
        .trim_start_matches("Error: stepping, ")
        .trim_start_matches("Error: ")
        .trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .trim_end()
        .to_string()
}

#[test]
fn scalar_subquery_arity_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let s = "CREATE TABLE t(a,b);";
    for sql in [
        // A multi-column subquery in a scalar position — rejected at prepare time
        // even though the table is empty and the expression never evaluates.
        &format!("{s} SELECT (SELECT a, b FROM t) FROM t"),
        &format!("{s} SELECT 1 FROM t ORDER BY (SELECT 1, 2)"),
        &format!("{s} SELECT a + (SELECT 1, 2) FROM t"),
        &format!("{s} SELECT a || (SELECT 1, 2) FROM t"),
        &format!("{s} SELECT abs((SELECT 1, 2)) FROM t"),
        &format!("{s} SELECT -(SELECT 1, 2) FROM t"),
        &format!("{s} SELECT NOT (SELECT 1, 2) FROM t"),
        &format!("{s} SELECT CAST((SELECT 1, 2) AS INT) FROM t"),
        &format!("{s} SELECT * FROM t WHERE (SELECT 1, 2)"),
        &format!("{s} SELECT * FROM t WHERE a AND (SELECT 1, 2)"),
        &format!("{s} SELECT * FROM t WHERE a LIKE (SELECT 1, 2)"),
        &format!("{s} SELECT * FROM t WHERE (SELECT 1, 2) IS NULL"),
        &format!("{s} SELECT * FROM t WHERE a IN ((SELECT 1, 2))"),
        // Correlated, still arity-checked at prepare time.
        &format!("{s} SELECT (SELECT a, b FROM t x WHERE x.a=t.a) FROM t"),
        // The UPDATE/DELETE SET and WHERE paths reject it the same way.
        &format!("{s} UPDATE t SET a=(SELECT 1, 2)"),
        &format!("{s} UPDATE t SET a=1 WHERE (SELECT 1, 2)"),
        &format!("{s} DELETE FROM t WHERE (SELECT 1, 2)"),
        &format!("{s} DELETE FROM t WHERE a + (SELECT 1, 2)"),
        // A single-column subquery is valid — no error, empty result.
        &format!("{s} SELECT (SELECT a FROM t) FROM t"),
        &format!("{s} SELECT a + (SELECT b FROM t) FROM t"),
        // And a single-column scalar subquery over a non-empty table still runs.
        &format!("{s} INSERT INTO t VALUES(1,2); SELECT (SELECT a FROM t) FROM t"),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
