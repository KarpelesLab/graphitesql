//! A `%` that is the final character of a `printf`/`format` format string — with
//! no conversion specifier following — is emitted literally by SQLite
//! (`printf('%')` → "%", `printf('abc%')` → "abc%", `printf('%d%', 5)` → "5%").
//! graphite previously dropped the dangling `%`. A `%` followed by flags or a
//! width that then runs off the end (`%5`, `%-`) still produces nothing, and
//! that case is unchanged. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn text(c: &Connection, sql: &str) -> String {
    match c.query(sql).unwrap().rows.remove(0).remove(0) {
        Value::Text(s) => s,
        other => panic!("expected text, got {other:?} for {sql}"),
    }
}

#[test]
fn trailing_percent_is_literal() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT printf('%')"), "%");
    assert_eq!(text(&c, "SELECT printf('abc%')"), "abc%");
    assert_eq!(text(&c, "SELECT printf('a%')"), "a%");
    assert_eq!(text(&c, "SELECT printf('%', 1)"), "%");
    assert_eq!(text(&c, "SELECT format('%')"), "%");
    // A trailing `%` after a completed conversion is still literal.
    assert_eq!(text(&c, "SELECT printf('%d%', 5)"), "5%");
}

#[test]
fn other_percent_forms_unchanged() {
    let c = Connection::open_memory().unwrap();
    // `%%` is one literal percent.
    assert_eq!(text(&c, "SELECT printf('%%')"), "%");
    assert_eq!(text(&c, "SELECT printf('100%% done')"), "100% done");
    // A `%` with trailing flags/width but no conversion specifier yields nothing
    // past the literal prefix.
    assert_eq!(text(&c, "SELECT printf('x%5')"), "x");
    assert_eq!(text(&c, "SELECT printf('a%-')"), "a");
    assert_eq!(text(&c, "SELECT printf('%d', 5)"), "5");
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
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    };
    for sql in [
        "SELECT printf('%')",
        "SELECT printf('abc%')",
        "SELECT printf('a%')",
        "SELECT printf('%', 1)",
        "SELECT format('%')",
        "SELECT printf('%d%', 5)",
        "SELECT printf('%%')",
        "SELECT printf('100%% done')",
        "SELECT printf('x%5')",
        "SELECT printf('a%-')",
        "SELECT printf('%d', 5)",
        "SELECT printf('')",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
