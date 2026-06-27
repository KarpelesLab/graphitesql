//! `ORDER BY` / `LIMIT` bind to the *whole* compound query, never an inner arm.
//! Writing one before a `UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT`
//! (`SELECT … ORDER BY … UNION SELECT …`) is rejected by SQLite at prepare time
//! with `ORDER BY clause should come after <OP> not before` (or `LIMIT clause
//! …` when only a `LIMIT` is misplaced — `ORDER BY` wins when both are present).
//! graphite used to leave the operator unconsumed and report a bare
//! `near "UNION": syntax error`. Verified against the sqlite3 3.50.4 CLI.

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
fn order_by_or_limit_before_compound_op_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        // ORDER BY before each compound operator, named in the message.
        "SELECT 1 ORDER BY 1 UNION SELECT 2",
        "SELECT 1 ORDER BY 1 UNION ALL SELECT 2",
        "SELECT 1 ORDER BY 1 INTERSECT SELECT 2",
        "SELECT 1 ORDER BY 1 EXCEPT SELECT 2",
        // LIMIT before a compound operator (no ORDER BY) → LIMIT message.
        "SELECT 1 LIMIT 1 UNION SELECT 2",
        "SELECT 1 LIMIT 1 OFFSET 2 UNION SELECT 2",
        // Both present → ORDER BY takes precedence.
        "SELECT 1 ORDER BY 1 LIMIT 1 UNION SELECT 2",
        // Misplaced clause on a *middle* arm of a 3-way compound.
        "SELECT 1 UNION SELECT 2 ORDER BY 1 UNION SELECT 3",
        // Correct placement still parses: trailing ORDER BY / LIMIT bind the
        // whole compound.
        "SELECT 1 UNION SELECT 2 ORDER BY 1",
        "SELECT 1 UNION SELECT 2 LIMIT 1",
        "SELECT 1 UNION SELECT 2 ORDER BY 1 LIMIT 1",
        "SELECT 2 ORDER BY 1 LIMIT 1",
        "VALUES(1),(2) ORDER BY 1",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
