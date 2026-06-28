//! A window function may not appear inside *another* window function's
//! definition — its arguments, its `FILTER` predicate, or its `OVER`
//! specification's `PARTITION BY` / `ORDER BY` (including a named `WINDOW w AS
//! (…)` referenced via `OVER w`). SQLite rejects this at prepare time as
//! `misuse of window function <inner>()`, even over an empty table; graphite
//! evaluated lazily and silently accepted it. An ordinary aggregate in those
//! same spots stays legal (`OVER (ORDER BY count(*))`), and a window function in
//! a frame-bound offset is the separate lazy "frame offset must be a
//! non-negative integer" path — both are exercised here as the valid/divergent
//! controls.
//!
//! Verified against the sqlite3 3.50.4 CLI.

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
fn window_in_window_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let t = "CREATE TABLE t(a,b);";
    let p = "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(3,4);";
    for sql in [
        // Window nested in another window's OVER spec — rejected even when empty.
        &format!("{t} SELECT row_number() OVER (ORDER BY sum(a) OVER ()) FROM t"),
        &format!("{t} SELECT row_number() OVER (PARTITION BY sum(a) OVER ()) FROM t"),
        &format!("{t} SELECT sum(a) OVER (ORDER BY row_number() OVER ()) FROM t"),
        &format!("{t} SELECT sum(a) OVER (ORDER BY count(*) OVER ()) FROM t"),
        // Window nested in another window's argument or FILTER.
        &format!("{t} SELECT sum(row_number() OVER ()) OVER () FROM t"),
        &format!("{t} SELECT count(*) FILTER (WHERE sum(a) OVER () > 0) OVER () FROM t"),
        // Named window: the nested window lives in the WINDOW definition.
        &format!("{t} SELECT row_number() OVER w FROM t WINDOW w AS (ORDER BY sum(a) OVER ())"),
        &format!(
            "{t} SELECT row_number() OVER w FROM t WINDOW w AS (PARTITION BY count(*) OVER ())"
        ),
        // Same shapes over a populated table still error at prepare time.
        &format!("{p} SELECT row_number() OVER (ORDER BY sum(a) OVER ()) FROM t"),
        // Valid: an ordinary aggregate in the spec, and plain windows.
        &format!("{p} SELECT row_number() OVER (ORDER BY count(*)) FROM t"),
        &format!("{p} SELECT sum(a) OVER (ORDER BY b) FROM t"),
        &format!("{p} SELECT row_number() OVER (PARTITION BY b ORDER BY a) FROM t"),
        &format!("{p} SELECT row_number() OVER w FROM t WINDOW w AS (PARTITION BY b ORDER BY a)"),
        // A window function in a frame-bound offset is the separate frame-offset
        // path (lazy: errors only once a row is produced).
        &format!(
            "{t} SELECT sum(a) OVER (ORDER BY b ROWS (row_number() OVER ()) PRECEDING) FROM t"
        ),
        &format!(
            "{p} SELECT sum(a) OVER (ORDER BY b ROWS (row_number() OVER ()) PRECEDING) FROM t"
        ),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
