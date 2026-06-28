//! `FROM name(args)` where `name` is not a built-in table-valued function.
//! SQLite resolves the bare name as a table/view first: if such an object
//! exists, calling it with an argument list is `'<name>' is not a function`
//! (the schema qualifier, if any, is dropped); otherwise it is a plain missing
//! table, with the qualifier echoed as written (an unknown qualifier is still
//! `no such table: bad.t`, never `unknown database bad`). graphite uniformly
//! reported `no such table-valued function: <name>`; it now matches SQLite.
//!
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
fn table_function_name_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        // Name does not resolve to any object → missing table (qualifier kept).
        "SELECT * FROM nope()",
        "SELECT * FROM abs()",
        "SELECT * FROM main.t()",
        "SELECT * FROM bad.t()",
        "SELECT * FROM temp.t()",
        // Name resolves to a table/view → `'<name>' is not a function`.
        "CREATE TABLE t(a); SELECT * FROM t()",
        "CREATE TABLE t(a); SELECT * FROM t(1, 2)",
        "CREATE TABLE t(a); SELECT * FROM main.t()",
        "CREATE VIEW v AS SELECT 1; SELECT * FROM v()",
        "CREATE TEMP TABLE t(a); SELECT * FROM temp.t()",
        // The schema tables are real objects too.
        "SELECT * FROM sqlite_master()",
        // A genuine built-in TVF still works (no error).
        "SELECT value FROM generate_series(1, 3)",
        "SELECT value FROM json_each('[1]')",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
