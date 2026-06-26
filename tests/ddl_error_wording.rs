//! Two DDL error messages are aligned to the `sqlite3` CLI (3.50.4) verbatim:
//!
//!   * a `WITHOUT ROWID` table with no PRIMARY KEY is "PRIMARY KEY missing on
//!     table NAME" (graphite previously said "WITHOUT ROWID table must have a
//!     PRIMARY KEY");
//!   * `ALTER TABLE … RENAME COLUMN <missing> …` quotes the unknown column name,
//!     "no such column: \"NAME\"", like the DROP COLUMN variant already did.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn without_rowid_missing_pk_message() {
    let mut c = Connection::open_memory().unwrap();
    assert!(c
        .execute("CREATE TABLE t(a, b) WITHOUT ROWID")
        .unwrap_err()
        .to_string()
        .contains("PRIMARY KEY missing on table t"));
    // A WITHOUT ROWID table with a PRIMARY KEY is still fine.
    c.execute("CREATE TABLE ok(a PRIMARY KEY, b) WITHOUT ROWID")
        .unwrap();
}

#[test]
fn rename_missing_column_is_quoted() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    assert!(c
        .execute("ALTER TABLE t RENAME COLUMN nope TO x")
        .unwrap_err()
        .to_string()
        .contains("no such column: \"nope\""));
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, args: &[&str]| -> String {
        let out = Command::new(bin)
            .arg(":memory:")
            .args(args)
            .output()
            .unwrap();
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
    let cases: &[&[&str]] = &[
        &["CREATE TABLE t(a, b) WITHOUT ROWID"],
        &["CREATE TABLE t(a); ALTER TABLE t RENAME COLUMN nope TO x;"],
    ];
    for args in cases {
        assert_eq!(run("sqlite3", args), run(g, args), "for {args:?}");
    }
}
