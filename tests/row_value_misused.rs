//! A row value `(a, b, …)` is only legal in the contexts that accept one — a
//! row comparison (`(a,b) = (c,d)`, `(a,b) < (c,d)`) or `(a,b) IN (…)`. Used
//! anywhere a single scalar is expected (bare in the SELECT list, as an
//! arithmetic operand, a function argument, or a bare `WHERE` predicate), SQLite
//! reports `row value misused`. graphite previously used its own wording
//! ("row value used where a single value is expected"). Matched to the `sqlite3`
//! CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn graphite_err(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn row_value_in_scalar_context_is_misused() {
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT (1, 2)",
        "SELECT (1, 2, 3)",
        "SELECT (1, 2) + 1",
        "SELECT 1 WHERE (1, 2)",
        "SELECT 1 WHERE (1, 2) = 1",
        "SELECT max((1, 2))",
        "SELECT abs((1, 2))",
    ] {
        assert_eq!(graphite_err(&c, sql), "row value misused", "for {sql}");
    }

    // The legal row-value contexts are unaffected.
    assert_eq!(
        c.query("SELECT (1, 2) = (1, 2)").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("SELECT (1, 2) < (3, 4)").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("SELECT (1, 2) IN ((1, 2), (3, 4))").unwrap().rows[0][0],
        Value::Integer(1)
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let sqlite_err = |sql: &str| -> String {
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg(sql)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .to_string()
    };
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT (1, 2)",
        "SELECT (1, 2, 3)",
        "SELECT (1, 2) + 1",
        "SELECT 1 WHERE (1, 2)",
        "SELECT 1 WHERE (1, 2) = 1",
        "SELECT max((1, 2))",
        "SELECT abs((1, 2))",
    ] {
        assert_eq!(graphite_err(&c, sql), sqlite_err(sql), "for {sql}");
    }
}
