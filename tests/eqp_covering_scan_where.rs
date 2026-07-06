//! A no-seek `WHERE` scan reads from a *covering* index when one index holds
//! every column the query references (both projected and `WHERE`-tested) and is
//! narrower than the table: SQLite reads and filters in the index
//! (`SCAN t USING COVERING INDEX <ix>`) rather than the table. A `WHERE` that
//! *seeks* an index (an equality/range on the index's leading column) is a
//! `SEARCH` and is left to that path. Verified (plan and rows) vs sqlite3 3.50.4.

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
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(format!("{base} {sql}"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

const BASE: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c, d, e);\
    CREATE INDEX tbc ON t(b,c); CREATE INDEX td ON t(d);\
    INSERT INTO t VALUES(1,5,50,7,'x'),(2,3,30,9,'y'),(3,9,90,8,'z');";

#[test]
fn covering_scan_where_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        // Non-seekable WHERE fully covered by tbc → covering scan.
        "SELECT b FROM t WHERE c > 40",
        "SELECT b FROM t WHERE c = 50",
        "SELECT b, c FROM t WHERE c > 40",
        "SELECT b FROM t WHERE c BETWEEN 30 AND 60",
        "SELECT b FROM t WHERE c IN (30, 90)",
        "SELECT count(*) FROM t WHERE c > 40",
        "SELECT b FROM t WHERE c > 40 OR c < 10",
        // Seekable WHERE → SEARCH, not a covering full scan.
        "SELECT b FROM t WHERE b = 5",
        "SELECT b FROM t WHERE c > 40 AND b < 9",
        "SELECT b FROM t WHERE c > 40 AND d = 7",
        // Not covered (d in tbc? no) → plain scan.
        "SELECT b FROM t WHERE e > 0",
    ] {
        assert_eq!(plan("sqlite3", BASE, q), plan(g, BASE, q), "plan `{q}`");
        assert_eq!(rows("sqlite3", BASE, q), rows(g, BASE, q), "rows `{q}`");
    }
}
