//! A partial-index predicate (`CREATE INDEX … WHERE p`) may reference only the
//! target table's own columns (plus its rowid). An unknown column is rejected at
//! CREATE with `no such column: NAME`, matching SQLite — graphite previously
//! built the index silently, leaving a predicate that could never be evaluated.
//! Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn unknown_column_in_partial_predicate_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    assert!(c
        .execute("CREATE INDEX i ON t(a) WHERE unknown_col > 0")
        .unwrap_err()
        .to_string()
        .contains("no such column: unknown_col"));
    // The bad reference is found even when nested inside a function call and ANDed
    // with a valid term.
    assert!(c
        .execute("CREATE INDEX i ON t(a) WHERE a > 0 AND abs(nope) < 5")
        .unwrap_err()
        .to_string()
        .contains("no such column: nope"));
}

#[test]
fn valid_partial_predicates_still_build() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    // Own columns, a function of an own column, and the rowid are all fine.
    c.execute("CREATE INDEX i1 ON t(a) WHERE b > 0").unwrap();
    c.execute("CREATE INDEX i2 ON t(a) WHERE abs(b) > 0")
        .unwrap();
    c.execute("CREATE INDEX i3 ON t(a) WHERE rowid > 0")
        .unwrap();
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str| -> String {
        let out = Command::new(bin)
            .arg(":memory:")
            .arg("CREATE TABLE t(a, b);")
            .arg("CREATE INDEX i ON t(a) WHERE unknown_col > 0;")
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
    assert_eq!(run("sqlite3"), run(g));
}
