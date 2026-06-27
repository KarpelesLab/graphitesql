//! A scalar `x [NOT] IN (SELECT …)` requires the subquery to return exactly one
//! column. SQLite rejects `1 IN (SELECT 1, 2)` with "sub-select returns 2
//! columns - expected 1" (the same message its row-value `IN` and scalar
//! `(SELECT …)` paths already produce). graphite previously collapsed the
//! subquery to its first column and silently accepted the extra columns.
//! Single-column subqueries — including multi-row and table-backed ones — are
//! unaffected. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn err(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn multi_column_in_subquery_is_rejected() {
    let c = Connection::open_memory().unwrap();
    for (sql, n) in [
        ("SELECT 1 WHERE 1 IN (SELECT 1, 2)", 2),
        ("SELECT 1 WHERE 1 NOT IN (SELECT 1, 2)", 2),
        ("SELECT 5 IN (SELECT 1, 2, 3)", 3),
        ("SELECT 9 IN (SELECT 1, 2 UNION SELECT 3, 4)", 2),
    ] {
        assert_eq!(
            err(&c, sql),
            format!("sub-select returns {n} columns - expected 1"),
            "for {sql}"
        );
    }
}

#[test]
fn single_column_in_subquery_still_works() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    let one = |sql: &str| c.query(sql).unwrap().rows.remove(0).remove(0);
    assert_eq!(one("SELECT 1 IN (SELECT 1)"), Value::Integer(1));
    assert_eq!(
        one("SELECT 3 IN (SELECT 1 UNION SELECT 3)"),
        Value::Integer(1)
    );
    assert_eq!(
        one("SELECT 9 IN (SELECT 1 UNION SELECT 3)"),
        Value::Integer(0)
    );
    assert_eq!(
        one("SELECT 2 NOT IN (SELECT 1 UNION SELECT 3)"),
        Value::Integer(1)
    );
    assert_eq!(one("SELECT 2 IN (SELECT a FROM t)"), Value::Integer(1));
    assert_eq!(one("SELECT 7 IN (SELECT a FROM t)"), Value::Integer(0));
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
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "SELECT 1 WHERE 1 IN (SELECT 1, 2)",
        "SELECT 1 WHERE 1 NOT IN (SELECT 1, 2)",
        "SELECT 5 IN (SELECT 1, 2, 3)",
        "SELECT 9 IN (SELECT 1, 2 UNION SELECT 3, 4)",
        "SELECT 1 IN (SELECT 1)",
        "SELECT 3 IN (SELECT 1 UNION SELECT 3)",
        "SELECT 9 IN (SELECT 1 UNION SELECT 3)",
        "SELECT 2 NOT IN (SELECT 1 UNION SELECT 3)",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
