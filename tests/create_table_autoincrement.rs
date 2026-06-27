//! `AUTOINCREMENT` is a reserved keyword that is only legal immediately after a
//! column-level `PRIMARY KEY [ASC|DESC]`. Anywhere else — as a bare constraint,
//! or swallowed as part of a type name — SQLite raises
//! `near "AUTOINCREMENT": syntax error`. graphite used to accept `a AUTOINCREMENT`
//! / `a INTEGER AUTOINCREMENT` / `a AUTOINCREMENT PRIMARY KEY` by treating the
//! keyword as a type word. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn err(c: &mut Connection, sql: &str) -> String {
    c.execute(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .trim_start_matches("SQL error: ")
        .to_string()
}

#[test]
fn autoincrement_outside_primary_key_is_a_syntax_error() {
    for ddl in [
        "CREATE TABLE t(a AUTOINCREMENT)",
        "CREATE TABLE t(a INTEGER AUTOINCREMENT)",
        "CREATE TABLE t(a AUTOINCREMENT PRIMARY KEY)",
        "CREATE TABLE t(a UNIQUE AUTOINCREMENT)",
        "CREATE TABLE t(a NOT NULL AUTOINCREMENT)",
    ] {
        let mut c = Connection::open_memory().unwrap();
        assert_eq!(
            err(&mut c, ddl),
            "near \"AUTOINCREMENT\": syntax error",
            "for {ddl}"
        );
    }
}

#[test]
fn autoincrement_after_primary_key_still_works() {
    // An INTEGER PRIMARY KEY AUTOINCREMENT parses and builds.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b)")
        .unwrap();
    // A non-INTEGER PRIMARY KEY AUTOINCREMENT parses fine but is the usual
    // semantic error — not a syntax error (and exactly what SQLite reports).
    let mut c = Connection::open_memory().unwrap();
    assert_eq!(
        err(&mut c, "CREATE TABLE t(a PRIMARY KEY AUTOINCREMENT)"),
        "AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY"
    );
    // A quoted type name that happens to be the keyword is still a plain type.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a \"autoincrement\")").unwrap();
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
        "CREATE TABLE t(a AUTOINCREMENT);",
        "CREATE TABLE t(a INTEGER AUTOINCREMENT);",
        "CREATE TABLE t(a AUTOINCREMENT PRIMARY KEY);",
        "CREATE TABLE t(a UNIQUE AUTOINCREMENT);",
        "CREATE TABLE t(a NOT NULL AUTOINCREMENT);",
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b); INSERT INTO t(b) VALUES(1); SELECT a FROM t;",
        "CREATE TABLE t(a PRIMARY KEY AUTOINCREMENT); PRAGMA table_info(t);",
        "CREATE TABLE t(a \"autoincrement\"); PRAGMA table_info(t);",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
