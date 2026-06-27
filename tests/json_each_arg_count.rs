//! `json_each` / `json_tree` argument-count handling, matched to the sqlite3
//! 3.50.4 CLI:
//!   * no argument at all behaves like a NULL document — zero rows, no error
//!     (graphite previously raised `json_each() requires a JSON argument`);
//!   * more than two arguments is a structural error,
//!     `too many arguments on json_each() - max 2` (graphite previously ignored
//!     the extras and silently produced rows).

#![cfg(feature = "std")]

use graphitesql::Connection;
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
fn no_argument_yields_no_rows() {
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT * FROM json_each()",
        "SELECT * FROM json_tree()",
        "SELECT * FROM json_each(NULL)",
        "SELECT * FROM json_tree(NULL)",
    ] {
        assert!(c.query(sql).unwrap().rows.is_empty(), "for {sql}");
    }
}

#[test]
fn more_than_two_arguments_is_rejected() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        err(&c, "SELECT * FROM json_each(1,2,3)"),
        "too many arguments on json_each() - max 2"
    );
    assert_eq!(
        err(&c, "SELECT * FROM json_tree(1,2,3)"),
        "too many arguments on json_tree() - max 2"
    );
    // The cap is on the count, not the values — a valid doc + path + extra still
    // fails before any rows are produced.
    assert_eq!(
        err(&c, "SELECT * FROM json_each('[1]','$',3)"),
        "too many arguments on json_each() - max 2"
    );
}

#[test]
fn valid_one_and_two_argument_forms_still_work() {
    let c = Connection::open_memory().unwrap();
    let n = |sql: &str| -> i64 {
        match c.query(sql).unwrap().rows.remove(0).remove(0) {
            graphitesql::Value::Integer(i) => i,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(n("SELECT count(*) FROM json_each('[1,2,3]')"), 3);
    assert_eq!(n("SELECT count(*) FROM json_each('{\"a\":1,\"b\":2}')"), 2);
    assert_eq!(
        n("SELECT count(*) FROM json_each('{\"a\":[1,2]}', '$.a')"),
        2
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
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "SELECT * FROM json_each()",
        "SELECT * FROM json_tree()",
        "SELECT * FROM json_each(1,2,3)",
        "SELECT * FROM json_tree(1,2,3)",
        "SELECT * FROM json_each('[1]','$',3)",
        "SELECT count(*) FROM json_each('[1,2,3]')",
        "SELECT count(*) FROM json_tree('{\"a\":[1,2]}')",
        "SELECT * FROM json_each(NULL)",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
