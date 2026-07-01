//! B9f — a fixed-prefix `GLOB` pattern seeks a byte range on a BINARY index. SQLite's
//! `GLOB` is always case-sensitive and byte-based, so `b GLOB 'abc*'` matches exactly
//! `b >= 'abc' AND b < 'abd'` and reads `SEARCH t USING INDEX tb (b>? AND b<?)`; a
//! leading wildcard (`'*abc'`, `'[ab]c'`) has no seekable prefix and scans. The seek
//! only applies to a BINARY-collation column (a NOCASE index orders keys differently),
//! and the full `GLOB` is re-applied so results are exact. graphite used to scan every
//! `GLOB`. Verified vs the sqlite3 3.50.4 CLI.

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

const BINARY_COL: &str =
    "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c); CREATE INDEX tb ON t(b);";
const NOCASE_COL: &str =
    "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT COLLATE NOCASE, c); CREATE INDEX tb ON t(b);";

#[test]
fn glob_prefix_plan_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM t WHERE b GLOB 'abc*'", // prefix + wildcard → range seek
        "SELECT * FROM t WHERE b GLOB 'abc'",  // no wildcard → still a prefix range
        "SELECT * FROM t WHERE b GLOB 'a?c*'", // '?' stops the prefix at 'a'
        "SELECT * FROM t WHERE b GLOB '*abc'", // leading wildcard → no prefix → SCAN
        "SELECT * FROM t WHERE b GLOB '[ab]c'", // char class → no prefix → SCAN
    ] {
        assert_eq!(
            plan("sqlite3", BINARY_COL, q),
            plan(g, BINARY_COL, q),
            "plan for {q}"
        );
    }
    // A NOCASE-collation column can't serve the byte-ordered GLOB range → SCAN.
    assert_eq!(
        plan("sqlite3", NOCASE_COL, "SELECT * FROM t WHERE b GLOB 'abc*'"),
        plan(g, NOCASE_COL, "SELECT * FROM t WHERE b GLOB 'abc*'"),
    );
}

#[test]
fn glob_prefix_rows_match() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = format!(
        "{BINARY_COL} INSERT INTO t VALUES\
         (1,'abc',1),(2,'abd',2),(3,'abcd',3),(4,'ABC',4),(5,'abz',5),(6,'xabc',6),(7,'ab',7);"
    );
    for q in [
        "SELECT b FROM t WHERE b GLOB 'abc*' ORDER BY b", // case-sensitive: excludes ABC
        "SELECT b FROM t WHERE b GLOB 'ab*' ORDER BY b",
        "SELECT b FROM t WHERE b GLOB 'abc' ORDER BY b",
        "SELECT count(*) FROM t WHERE b GLOB 'a*'",
        "SELECT b FROM t WHERE b GLOB 'zzz*' ORDER BY b", // empty range
    ] {
        assert_eq!(rows("sqlite3", &base, q), rows(g, &base, q), "rows for {q}");
    }
}
