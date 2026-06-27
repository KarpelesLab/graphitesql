//! A trigger body that targets a missing table reports the error qualified by
//! the *trigger's* schema, the way SQLite compiles a trigger program in its own
//! database: a `main` trigger says `no such table: main.nope`, while a *temp*
//! trigger — whose names resolve across all schemas — keeps the bare
//! `no such table: nope`. graphite used to report the bare name in every case.
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// First non-caret line of combined stdout/stderr, error-prefix stripped.
fn run(bin: &str, sql: &str) -> String {
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
}

#[test]
fn trigger_body_missing_table_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        // main trigger → schema-qualified `main.nope`
        "CREATE TABLE t(a); CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO nope VALUES(1); END; INSERT INTO t VALUES(1)",
        "CREATE TABLE t(a); CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE nope SET x=1; END; INSERT INTO t VALUES(1)",
        "CREATE TABLE t(a); CREATE TRIGGER tr AFTER INSERT ON t BEGIN DELETE FROM nope; END; INSERT INTO t VALUES(1)",
        "CREATE TABLE t(a); CREATE TRIGGER tr BEFORE INSERT ON t BEGIN INSERT INTO nope VALUES(1); END; INSERT INTO t VALUES(1)",
        "CREATE TABLE t(a); CREATE TRIGGER tr AFTER UPDATE ON t BEGIN DELETE FROM nope; END; INSERT INTO t VALUES(1); UPDATE t SET a=2",
        "CREATE TABLE t(a); CREATE TRIGGER tr AFTER DELETE ON t BEGIN INSERT INTO nope VALUES(1); END; INSERT INTO t VALUES(1); DELETE FROM t",
        // temp trigger (on a main or temp table) → bare `nope`
        "CREATE TABLE t(a); CREATE TEMP TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO nope VALUES(1); END; INSERT INTO t VALUES(1)",
        "CREATE TEMP TABLE t(a); CREATE TEMP TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO nope VALUES(1); END; INSERT INTO t VALUES(1)",
        // a present target still works — the trigger writes the other table
        "CREATE TABLE t(a); CREATE TABLE u(b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO u VALUES(1); END; INSERT INTO t VALUES(1); SELECT count(*) FROM u",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
