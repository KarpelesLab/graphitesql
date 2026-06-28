//! A compound (`UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`) whose arms disagree in
//! column count is an error in both engines, but SQLite picks the *message* by
//! the shape of the right operand: when the right-hand arm of the mismatching
//! step is a `VALUES` clause it reports `all VALUES must have the same number of
//! terms` (regardless of the operator, and even if the left arm is an ordinary
//! `SELECT`); otherwise it names the operator — `SELECTs to the left and right
//! of UNION do not have the same number of result columns`. graphite previously
//! emitted the operator-named message for every `VALUES`-on-the-right mismatch
//! except the all-`UNION ALL` case. It now matches SQLite in every case.
//!
//! Verified against the sqlite3 3.50.4 CLI.

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
fn values_compound_arity_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        // Right operand is VALUES → the VALUES-specific message, whatever the
        // operator and whatever the left arm is.
        "VALUES (1,2) UNION VALUES (1)",
        "VALUES (1) UNION VALUES (1,2)",
        "SELECT 1,2 UNION VALUES (1)",
        "SELECT 1 UNION VALUES (1,2)",
        "VALUES (1,2) UNION ALL VALUES (1)",
        "VALUES (1,2) EXCEPT VALUES (1)",
        "VALUES (1,2) INTERSECT VALUES (1)",
        "VALUES (1,2) UNION VALUES (1) UNION VALUES (1,2,3)",
        "CREATE TABLE t(a,b); SELECT a,b FROM t UNION VALUES (1)",
        "CREATE TABLE t(a,b); SELECT a FROM t UNION VALUES (1,2)",
        // A single multi-row VALUES with an internal arity mismatch.
        "VALUES (1,2),(3)",
        // Right operand is a SELECT → the operator-named message.
        "VALUES (1,2) UNION SELECT 1",
        "SELECT 1 UNION SELECT 1,2",
        "SELECT 1,2,3 INTERSECT SELECT 1",
        // Equal-arity compounds still run and produce the same rows.
        "VALUES (1,2) UNION ALL VALUES (3,4)",
        "VALUES (1,2) UNION VALUES (3,4)",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
