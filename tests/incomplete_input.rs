//! When SQL ends prematurely — a valid prefix that runs out of tokens mid-parse
//! (`SELECT`, `SELECT 1 WHERE 1=1 AND`, `CREATE TABLE t(`) — SQLite reports a
//! single message: `incomplete input`. This is distinct from a syntax error at a
//! real token (`near "X": syntax error`). graphite previously surfaced its
//! internal parser state (`expected an expression, found None`, `expected
//! LParen (at end of input)`, …); every premature-EOF parse now reports
//! `incomplete input` to match. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The library renders a parse error as `SQL error: <msg>`; return just `<msg>`.
fn parse_msg(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("SQL error: ")
        .to_string()
}

#[test]
fn premature_eof_is_incomplete_input() {
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT",
        "SELECT 1 +",
        "SELECT 1 WHERE 1=1 AND",
        "SELECT 1 FROM",
        "SELECT * FROM t WHERE",
        "SELECT 1,",
        "SELECT 1 ORDER BY",
        "SELECT 1 GROUP BY",
        "SELECT 1 LIMIT",
        "SELECT 1 LIMIT 1 OFFSET",
        "SELECT 1 UNION",
        "SELECT (",
        "SELECT CASE WHEN 1 THEN",
        "INSERT INTO t",
        "INSERT INTO t VALUES",
        "CREATE TABLE",
        "CREATE TABLE t",
        "CREATE TABLE t(",
        "CREATE TABLE t(a,",
        "CREATE INDEX i ON",
        "UPDATE t SET",
        "DELETE FROM",
        "ALTER TABLE t",
        "WITH",
    ] {
        assert_eq!(parse_msg(&c, sql), "incomplete input", "for {sql}");
    }
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "SELECT",
        "SELECT 1 +",
        "SELECT 1 WHERE 1=1 AND",
        "SELECT 1 FROM",
        "SELECT 1,",
        "SELECT 1 ORDER BY",
        "SELECT 1 LIMIT",
        "SELECT 1 UNION",
        "SELECT (",
        "SELECT CASE WHEN 1 THEN",
        "INSERT INTO t",
        "INSERT INTO t VALUES",
        "CREATE TABLE",
        "CREATE TABLE t",
        "CREATE TABLE t(",
        "CREATE INDEX i ON",
        "UPDATE t SET",
        "DELETE FROM",
        "ALTER TABLE t",
        "WITH",
        // a complete statement still parses (no false "incomplete input")
        "SELECT 1",
        "CREATE TABLE t(a, b)",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
