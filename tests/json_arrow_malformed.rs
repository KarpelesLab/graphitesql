//! The `->` and `->>` JSON operators treat their left operand as a JSON
//! document. When that operand is a text (or numeric) value that is not valid
//! JSON, SQLite raises `malformed JSON` — exactly as `json_extract` does —
//! rather than silently yielding NULL. graphite previously swallowed the parse
//! failure and returned NULL for `'' -> 1`, `'notjson' -> 0`, `'{bad' -> 'a'`.
//!
//! A BLOB operand is SQLite's binary JSONB, which the arrow operators do not yet
//! model; that path is intentionally left lenient here and is not asserted.
//! Verified against the sqlite3 3.50.4 CLI.

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
fn malformed_text_document_is_an_error() {
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT '' -> 1",
        "SELECT '' ->> 1",
        "SELECT 'notjson' -> 0",
        "SELECT 'notjson' ->> 0",
        "SELECT '{bad' -> 'a'",
        "SELECT '[1,2' ->> 0",
    ] {
        assert_eq!(err(&c, sql), "malformed JSON", "for {sql}");
    }
}

#[test]
fn well_formed_documents_still_extract() {
    let c = Connection::open_memory().unwrap();
    let one = |sql: &str| c.query(sql).unwrap().rows.remove(0).remove(0);
    // Valid JSON text extracts as before.
    assert_eq!(one("SELECT '[1,2,3]' ->> 1"), Value::Integer(2));
    assert_eq!(one("SELECT '{\"a\":5}' ->> 'a'"), Value::Integer(5));
    assert_eq!(one("SELECT '  [9]  ' ->> 0"), Value::Integer(9));
    // A numeric document is a valid JSON scalar — no error, just a missed lookup.
    assert_eq!(one("SELECT 5 -> 0"), Value::Null);
    assert_eq!(one("SELECT 3.14 -> 0"), Value::Null);
    // A NULL document short-circuits to NULL.
    assert_eq!(one("SELECT NULL -> 0"), Value::Null);
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
        "SELECT '' -> 1",
        "SELECT '' ->> 1",
        "SELECT 'notjson' -> 0",
        "SELECT 'notjson' ->> 0",
        "SELECT '{bad' -> 'a'",
        "SELECT '[1,2,3]' -> 0",
        "SELECT '[1,2,3]' ->> 2",
        "SELECT '{\"a\":5}' ->> 'a'",
        "SELECT 5 -> 0",
        "SELECT 3.14 -> 0",
        "SELECT '\"hi\"' ->> '$'",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
