//! A `FILTER (WHERE …)` predicate is resolved as an ordinary boolean expression
//! that may not itself aggregate, so an aggregate or window function nested inside
//! it is a misuse: `count(*) FILTER (WHERE sum(a)>0)` → `misuse of aggregate
//! function sum()`, `… FILTER (WHERE rank()>0)` → `misuse of window function
//! rank()`. SQLite raises this at prepare time; graphite evaluated the filter
//! lazily per row and so silently returned a value over an empty (or fully
//! filtered) table. A missing column inside the filter is still caught first
//! (`no such column: zzz`). The windowed carrier `count(*) FILTER (…) OVER ()` is
//! exempt — SQLite accepts it — so it is not turned into a misuse. Verified
//! against the sqlite3 3.50.4 CLI.

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
fn filter_aggregate_misuse_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let s = "CREATE TABLE t(a,b);";
    for sql in [
        // An aggregate inside the FILTER predicate is a misuse, raised at prepare
        // time even though the table is empty (so the filter never runs).
        &format!("{s} SELECT count(*) FILTER (WHERE sum(a)>0) FROM t"),
        &format!("{s} SELECT count(*) FILTER (WHERE max(a)>0) FROM t"),
        &format!("{s} SELECT count(*) FILTER (WHERE total(b)>0) FROM t"),
        &format!("{s} SELECT sum(a) FILTER (WHERE count(*)>0) FROM t"),
        // Nested inside another expression in the predicate.
        &format!("{s} SELECT count(*) FILTER (WHERE 1+count(b)>0) FROM t"),
        &format!("{s} SELECT count(*) FILTER (WHERE abs(sum(a))>0) FROM t"),
        // A window function inside the predicate — with OVER and the window-only
        // builtin without OVER.
        &format!("{s} SELECT count(*) FILTER (WHERE row_number() OVER ()>0) FROM t"),
        &format!("{s} SELECT count(*) FILTER (WHERE rank()>0) FROM t"),
        // In a HAVING-clause aggregate's filter too.
        &format!("{s} SELECT count(*) FROM t GROUP BY a HAVING sum(b) FILTER (WHERE count(*)>0)>0"),
        // A missing column inside the filter wins.
        &format!("{s} SELECT count(*) FILTER (WHERE zzz>0) FROM t"),
        &format!("{s} SELECT count(*) FILTER (WHERE sum(zzz)>0) FROM t"),
        // Valid filters still run (empty and non-empty), and are not misused.
        &format!("{s} SELECT count(*) FILTER (WHERE a>0) FROM t"),
        &format!("{s} INSERT INTO t VALUES(1,2),(3,4); SELECT count(*) FILTER (WHERE a>1) FROM t"),
        &format!("{s} INSERT INTO t VALUES(1,2); SELECT count(*) FILTER (WHERE sum(a)>0) FROM t"),
        // A non-aggregate carrier of FILTER is a different, already-handled error.
        &format!("{s} SELECT abs(a) FILTER (WHERE a>0) FROM t"),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
