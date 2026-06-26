//! A column `DEFAULT` must be constant: SQLite allows literals, `CURRENT_*`, and
//! function calls (deterministic or not), but rejects any reference to a column
//! at CREATE / ADD COLUMN time with `default value of column [NAME] is not
//! constant`. graphite previously accepted a column-referencing default silently.
//! Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn column_referencing_default_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    for sql in [
        "CREATE TABLE t(a, b DEFAULT (a))",
        "CREATE TABLE t(a, b DEFAULT (a + 1))",
        "CREATE TABLE t(a, b DEFAULT (max(a, 1)))",
    ] {
        assert!(
            c.execute(sql)
                .unwrap_err()
                .to_string()
                .contains("default value of column [b] is not constant"),
            "for {sql}"
        );
    }
    // The same rule applies to ALTER TABLE … ADD COLUMN.
    c.execute("CREATE TABLE u(a)").unwrap();
    assert!(c
        .execute("ALTER TABLE u ADD COLUMN b DEFAULT (a)")
        .unwrap_err()
        .to_string()
        .contains("default value of column [b] is not constant"));
}

#[test]
fn constant_defaults_are_accepted() {
    let mut c = Connection::open_memory().unwrap();
    // Literals, constant arithmetic, function calls, CURRENT_*, and NULL are all
    // constant in SQLite's sense.
    c.execute(
        "CREATE TABLE t(\
           a DEFAULT 5, \
           b DEFAULT (5 + 3), \
           c DEFAULT (abs(-1)), \
           d DEFAULT current_timestamp, \
           e DEFAULT (random()), \
           f DEFAULT NULL, \
           g DEFAULT (-5))",
    )
    .unwrap();
    c.execute("CREATE TABLE u(a)").unwrap();
    c.execute("ALTER TABLE u ADD COLUMN b DEFAULT (abs(-1))")
        .unwrap();
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
        "CREATE TABLE t(a, b DEFAULT (a))",
        "CREATE TABLE t(a, b DEFAULT (a + b))",
        "CREATE TABLE t(a, b DEFAULT (max(a, 1)))",
        "CREATE TABLE t(a, b DEFAULT 5)",
        "CREATE TABLE t(a, b DEFAULT (5 + 3))",
        "CREATE TABLE t(a, b DEFAULT (abs(-1)))",
        "CREATE TABLE t(a, b DEFAULT current_timestamp)",
        "CREATE TABLE t(a, b DEFAULT (random()))",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
