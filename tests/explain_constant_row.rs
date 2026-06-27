//! `EXPLAIN QUERY PLAN` of a `FROM`-less SELECT. SQLite scans a single synthetic
//! constant row and renders `\`--SCAN CONSTANT ROW`; graphite previously emitted
//! just the bare `QUERY PLAN` header (no scan node) for any no-FROM select. This
//! also covers a single-row `VALUES(...)`, which desugars to a no-FROM,
//! no-compound select. Multi-row `VALUES`, compound (`UNION`/…) queries, and
//! selects whose clauses contain a subquery render extra nodes graphite does not
//! model yet and are intentionally left alone. Verified vs the sqlite3 3.50.4 CLI.

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
fn from_less_select_scans_a_constant_row() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    assert_eq!(
        run(g, "EXPLAIN QUERY PLAN SELECT 1"),
        "QUERY PLAN\n`--SCAN CONSTANT ROW"
    );
    // A single-row VALUES desugars to the same shape.
    assert_eq!(
        run(g, "EXPLAIN QUERY PLAN VALUES(1)"),
        "QUERY PLAN\n`--SCAN CONSTANT ROW"
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        "EXPLAIN QUERY PLAN SELECT 1",
        "EXPLAIN QUERY PLAN SELECT 1+2 WHERE 1",
        "EXPLAIN QUERY PLAN SELECT max(1,2)",
        "EXPLAIN QUERY PLAN SELECT 1, 2, 3",
        "EXPLAIN QUERY PLAN SELECT 1 ORDER BY 1",
        "EXPLAIN QUERY PLAN SELECT 1 LIMIT 5 OFFSET 2",
        "EXPLAIN QUERY PLAN SELECT 'a' || 'b' AS x",
        "EXPLAIN QUERY PLAN VALUES(1)",
        "EXPLAIN QUERY PLAN VALUES('x','y')",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
