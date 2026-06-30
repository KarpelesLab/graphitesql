//! A parenthesized column in a `WHERE` constraint — `(a) = 2`, `(b) > 10`,
//! `(a) IN (1,2)`, `(b) IS NULL` — seeks its index exactly as the bare column does.
//! SQLite ignores the redundant parentheses; graphite previously failed to recognize
//! the column through the `Paren` wrapper and fell back to a full scan, so
//! `EXPLAIN QUERY PLAN` diverged (`SCAN` vs `SEARCH`). The fix lives in the shared
//! `col_index`, so the executor seek and the EQP label move in lockstep. Verified vs
//! the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn plan(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} EXPLAIN QUERY PLAN {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|c: char| " |`*+_-".contains(c)))
        .collect::<Vec<_>>()
        .join("#")
}

fn rows(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); CREATE INDEX tb ON t(b);";

#[test]
fn parenthesized_column_seeks_like_bare() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM t WHERE (a)=2",   // INTEGER PRIMARY KEY
        "SELECT * FROM t WHERE ((a))=2", // nested parens
        "SELECT * FROM t WHERE 2=(a)",   // flipped operands
        "SELECT * FROM t WHERE (b)=10",  // secondary index equality
        "SELECT * FROM t WHERE (b)>10",  // secondary index range
        "SELECT * FROM t WHERE (a) IN (1,2)",
        "SELECT * FROM t WHERE (b) IS NULL",
    ] {
        assert_eq!(
            plan("sqlite3", SCHEMA, q),
            plan(g, SCHEMA, q),
            "plan for {q}"
        );
    }
}

#[test]
fn parenthesized_column_rows_match() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = format!("{SCHEMA} INSERT INTO t VALUES(1,10,5),(2,20,5),(3,NULL,9),(4,40,1);");
    for q in [
        "SELECT a FROM t WHERE (a)=2",
        "SELECT a FROM t WHERE (b)>10 ORDER BY a",
        "SELECT a FROM t WHERE (a) IN (1,3) ORDER BY a",
        "SELECT a FROM t WHERE (b) IS NULL",
    ] {
        assert_eq!(rows("sqlite3", &base, q), rows(g, &base, q), "rows for {q}");
    }
}
