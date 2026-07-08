//! A parenthesized `VALUES` clause is a subquery expression, just like a
//! parenthesized `SELECT` — `(VALUES(1),(2))` is a scalar subquery yielding the
//! first row's first column. graphite's expression parser previously rejected
//! `VALUES` after `(` ("unexpected keyword VALUES in expression"). Matched to
//! the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn values_as_scalar_subquery() {
    let c = Connection::open_memory().unwrap();
    // A scalar subquery takes the first row, first column.
    assert_eq!(
        c.query("SELECT (VALUES(1),(2))").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("SELECT 1 + (VALUES(10))").unwrap().rows[0][0],
        Value::Integer(11)
    );
    assert_eq!(
        c.query("SELECT (VALUES('a'),('b')) || 'x'").unwrap().rows[0][0],
        Value::Text("ax".into())
    );
    // It composes inside IN, like a SELECT subquery.
    assert_eq!(
        c.query("SELECT 2 IN (VALUES(1),(2),(3))").unwrap().rows[0][0],
        Value::Integer(1)
    );
    // A multi-column VALUES used as a scalar is the same error as a SELECT.
    assert!(
        c.query("SELECT (VALUES(1,2))")
            .unwrap_err()
            .to_string()
            .contains("sub-select returns 2 columns")
    );
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
            .next()
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string()
    };
    for sql in [
        "SELECT (VALUES(1),(2))",
        "SELECT (VALUES(1))",
        "SELECT 1 + (VALUES(10))",
        "SELECT (VALUES('a'),('b')) || 'x'",
        "SELECT 2 IN (VALUES(1),(2),(3))",
        "SELECT (VALUES(1,2),(3,4))",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
