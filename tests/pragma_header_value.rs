//! `PRAGMA user_version = X` and `PRAGMA application_id = X` interpret their
//! argument the way SQLite does — as an integer token, not a SQL expression.
//! A bare identifier (`abc`) or any non-numeric text is parsed for a leading
//! signed-integer (with optional `0x` hex) prefix, defaulting to 0; it is never
//! an error. graphite previously evaluated the argument as an expression and
//! raised `no such column: abc`. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn uv(c: &Connection) -> i64 {
    match c.query("PRAGMA user_version").unwrap().rows[0][0] {
        Value::Integer(i) => i,
        ref other => panic!("user_version not integer: {other:?}"),
    }
}

#[test]
fn a_bare_identifier_value_is_a_no_op_not_an_error() {
    let mut c = Connection::open_memory().unwrap();
    // A non-numeric identifier leaves the version at its default 0.
    c.execute("PRAGMA user_version = abc").unwrap();
    assert_eq!(uv(&c), 0);
    c.execute("PRAGMA application_id = whatever").unwrap();
    // Setting it to a real value still works, and a later junk value reads as 0.
    c.execute("PRAGMA user_version = 42").unwrap();
    assert_eq!(uv(&c), 42);
    c.execute("PRAGMA user_version = nope").unwrap();
    assert_eq!(uv(&c), 0);
}

#[test]
fn integer_token_parsing_matches_sqlite() {
    // (argument, stored user_version) — the leading-int-prefix rule.
    let cases: &[(&str, i64)] = &[
        ("3", 3),
        ("-3", -3),
        ("5.9", 5),
        ("0x10", 16),
        ("'5'", 5),
        ("'12xy'", 12),
        ("'-4'", -4),
        ("''", 0),
        ("' 7 '", 0), // no leading-whitespace skip, unlike CAST
        ("abc", 0),
    ];
    for (arg, want) in cases {
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("PRAGMA user_version = {arg}")).unwrap();
        assert_eq!(uv(&c), *want, "for {arg}");
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
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    for arg in [
        "abc", "nope", "3", "-3", "5.9", "0x10", "'5'", "'12xy'", "'-4'", "''", "' 7 '",
    ] {
        let sql = format!("PRAGMA user_version = {arg}; PRAGMA user_version;");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {arg}");
        let sql = format!("PRAGMA application_id = {arg}; PRAGMA application_id;");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {arg}");
    }
}
