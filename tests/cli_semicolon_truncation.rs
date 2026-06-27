//! The CLI splits a batch on `;` before handing each piece to the engine. It
//! must re-attach the terminating `;` so the engine can distinguish a
//! `;`-truncated statement (`SELECT;` → `near ";": syntax error`) from a genuine
//! end-of-input truncation (`SELECT` with no `;` → `incomplete input`), exactly
//! as the sqlite3 CLI does. graphite's CLI previously stripped the `;`, so every
//! `;`-truncated statement was misreported as `incomplete input`. Verified
//! against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> String {
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
}

#[test]
fn semicolon_truncation_is_a_syntax_error_not_incomplete_input() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    assert_eq!(run(g, "SELECT;"), "near \";\": syntax error");
    assert_eq!(
        run(g, "SELECT * FROM sqlite_master WHERE;"),
        "near \";\": syntax error"
    );
    // A genuine end-of-input truncation (no `;`) still reports incomplete input.
    assert_eq!(run(g, "SELECT"), "incomplete input");
}

#[test]
fn valid_multi_statement_batches_still_run() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    assert_eq!(
        run(
            g,
            "CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT a FROM t;"
        ),
        "1"
    );
    // A trailing bare `;` is a no-op, not an error.
    assert_eq!(run(g, "SELECT 7;;"), "7");
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        "SELECT;",
        "SELECT",
        "SELECT * FROM sqlite_master WHERE;",
        "INSERT INTO x;",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT a FROM t;",
        "SELECT 7;;",
        "VALUES(1),;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
