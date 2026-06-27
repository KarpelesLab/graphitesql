//! Arity rules for the `json_set`/`json_insert`/`json_replace` family (and the
//! `jsonb_*` blob variants). SQLite is varargs-shaped here: zero arguments yield
//! `NULL`; an *even* argument count is a hard error whose message always names
//! the text-output `json_*` form — even for a `jsonb_*` call — as
//! `json_NAME() needs an odd number of arguments`; an *odd* count is a document
//! followed by zero or more `(path, value)` pairs, so a lone document is a no-op
//! that returns the document unchanged. graphite previously rejected both the
//! lone-document form and used a generic "requires a document and (path, value)
//! pairs" message. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The library's error message for `sql`, with the outer Display tag stripped.
fn err_msg(sql: &str) -> String {
    let c = Connection::open_memory().unwrap();
    let e = c.query(sql).unwrap_err().to_string();
    e.trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .to_string()
}

/// The single scalar value of a one-row, one-column query.
fn scalar(sql: &str) -> Value {
    let c = Connection::open_memory().unwrap();
    c.query(sql).unwrap().rows.into_iter().next().unwrap()[0].clone()
}

#[test]
fn zero_arguments_yield_null() {
    for f in ["json_set", "json_insert", "json_replace"] {
        assert_eq!(scalar(&format!("SELECT {f}()")), Value::Null, "{f}");
    }
}

#[test]
fn a_lone_document_is_a_no_op() {
    // An odd count with no (path, value) pairs returns the document unchanged.
    assert_eq!(
        scalar("SELECT json_replace('{\"a\":1}')"),
        Value::Text("{\"a\":1}".into())
    );
    assert_eq!(scalar("SELECT json_set('[]')"), Value::Text("[]".into()));
    // The jsonb_* variants return the JSONB blob form, not text.
    assert!(matches!(scalar("SELECT jsonb_set('{}')"), Value::Blob(_)));
}

#[test]
fn an_even_argument_count_is_rejected() {
    assert_eq!(
        err_msg("SELECT json_replace('{\"a\":1}','$.a')"),
        "json_replace() needs an odd number of arguments"
    );
    assert_eq!(
        err_msg("SELECT json_set('{}','$.a',1,'$.b')"),
        "json_set() needs an odd number of arguments"
    );
}

#[test]
fn jsonb_even_count_names_the_json_form() {
    // The blob variants borrow the text-output name in their error message.
    assert_eq!(
        err_msg("SELECT jsonb_replace('{}','$.a',1,'$.b')"),
        "json_replace() needs an odd number of arguments"
    );
    assert_eq!(
        err_msg("SELECT jsonb_insert('{}','$.a')"),
        "json_insert() needs an odd number of arguments"
    );
}

#[test]
fn ordinary_pairs_still_apply() {
    assert_eq!(
        scalar("SELECT json_set('{\"a\":1}','$.a',99)"),
        Value::Text("{\"a\":99}".into())
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
    };
    for sql in [
        "SELECT typeof(json_set())",
        "SELECT json_set('{}')",
        "SELECT json_replace('{\"a\":1}')",
        "SELECT json_replace('{\"a\":1}','$.a')",
        "SELECT json_set('{}','$.a',1,'$.b')",
        "SELECT jsonb_replace('{}','$.a',1,'$.b')",
        "SELECT jsonb_insert('{}','$.a')",
        "SELECT json_set('{\"a\":1}','$.a',99)",
        "SELECT hex(jsonb_set('{}'))",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
