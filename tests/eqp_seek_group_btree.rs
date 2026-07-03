//! B9d (unambiguous subset) — the `USE TEMP B-TREE FOR GROUP BY` / `FOR DISTINCT`
//! node materializes over a rowid *range* seek, not just a bare `SCAN`. A
//! `WHERE a>? GROUP BY c` (a the INTEGER PRIMARY KEY, c not the seek key) seeks the
//! rowid range — an unambiguous access path with no secondary-index *choice* — and
//! returns rows in rowid order, which is never the grouping order, so SQLite (and now
//! graphite) emit the grouping b-tree: `SEARCH t USING INTEGER PRIMARY KEY (rowid>?)#
//! USE TEMP B-TREE FOR GROUP BY`. graphite previously omitted the node under any seek.
//!
//! A rowid *equality* seek (single row → grouping is a no-op), a GROUP BY on the rowid
//! itself, and a secondary-index seek (where SQLite may pick a different composite
//! index — cost-model, roadmap B9h) all stay as before. Verified vs sqlite3 3.50.4.

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

// A single secondary index (on b) so the rowid-seek cases are unambiguous; c is
// deliberately un-indexed so no index serves the GROUP BY / DISTINCT.
const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); CREATE INDEX ib ON t(b);";

#[test]
fn rowid_range_seek_group_distinct_btree_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        // Now emit the b-tree over the rowid range seek.
        "SELECT c FROM t WHERE a>3 GROUP BY c",
        "SELECT c FROM t WHERE a>3 AND a<9 GROUP BY c",
        "SELECT DISTINCT c FROM t WHERE a>3",
        "SELECT c, count(*) FROM t WHERE a>=2 GROUP BY c",
        "SELECT c FROM t WHERE a>3 GROUP BY c ORDER BY c", // ORDER BY folded into the b-tree
        // Unchanged: rowid equality (single row → no b-tree), GROUP BY the rowid,
        // bare scan, and a secondary-index-served grouping.
        "SELECT c FROM t WHERE a=3 GROUP BY c",
        "SELECT c FROM t WHERE a>3 GROUP BY a",
        "SELECT c FROM t GROUP BY c",
        "SELECT DISTINCT c FROM t",
    ] {
        assert_eq!(
            plan("sqlite3", SCHEMA, q),
            plan(g, SCHEMA, q),
            "plan for {q}"
        );
    }
}

#[test]
fn rowid_range_seek_group_rows_match() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = format!(
        "{SCHEMA} INSERT INTO t VALUES(1,10,5),(2,20,5),(3,30,7),(4,40,5),(5,50,7),(6,60,9);"
    );
    for q in [
        "SELECT c FROM t WHERE a>3 GROUP BY c ORDER BY c",
        "SELECT DISTINCT c FROM t WHERE a>2 ORDER BY c",
        "SELECT c, count(*) FROM t WHERE a>1 GROUP BY c ORDER BY c",
    ] {
        assert_eq!(rows("sqlite3", &base, q), rows(g, &base, q), "rows for {q}");
    }
}
