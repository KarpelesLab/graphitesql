//! A `RETURNING` clause projects one row per modified row, so a `DELETE`/`UPDATE`
//! `RETURNING` is never an aggregate query and offers no window context. SQLite
//! rejects an aggregate or window function there at prepare time (`misuse of
//! aggregate function NAME()` / `misuse of window function NAME()`), and a
//! window-only builtin called without `OVER` is the same misuse. graphite
//! evaluated `RETURNING` lazily and so silently produced no rows over an empty
//! (fully deleted/updated) table; it now rejects these at prepare time. A
//! missing column is still reported first (`no such column: zzz`). `INSERT …
//! RETURNING` is — as in SQLite — *not* subject to this. Verified against the
//! sqlite3 3.50.4 CLI.

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
fn returning_aggregate_misuse_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let s = "CREATE TABLE t(a,b);";
    for sql in [
        // Aggregate functions in DELETE/UPDATE RETURNING are a misuse, raised at
        // prepare time even though the table is empty.
        &format!("{s} DELETE FROM t RETURNING count(*)"),
        &format!("{s} DELETE FROM t RETURNING sum(a)"),
        &format!("{s} DELETE FROM t RETURNING max(a)"),
        &format!("{s} DELETE FROM t RETURNING total(b)"),
        &format!("{s} UPDATE t SET a=1 RETURNING count(*)"),
        &format!("{s} UPDATE t SET a=1 RETURNING avg(a)"),
        // Nested inside another expression.
        &format!("{s} DELETE FROM t RETURNING abs(sum(a))"),
        &format!("{s} DELETE FROM t RETURNING 1+count(*)"),
        &format!("{s} UPDATE t SET a=1 RETURNING count(*)+b"),
        // Window functions — with OVER, and a window-only builtin without OVER.
        &format!("{s} DELETE FROM t RETURNING row_number() OVER ()"),
        &format!("{s} DELETE FROM t RETURNING row_number()"),
        &format!("{s} DELETE FROM t RETURNING rank()"),
        &format!("{s} UPDATE t SET a=1 RETURNING rank() OVER ()"),
        // Wrong argument count is still diagnosed (here, ahead of the misuse).
        &format!("{s} DELETE FROM t RETURNING ntile()"),
        // A missing column wins over the misuse.
        &format!("{s} DELETE FROM t RETURNING count(zzz)"),
        &format!("{s} DELETE FROM t RETURNING sum(a), zzz"),
        // INSERT … RETURNING is NOT subject to this (matches SQLite).
        &format!("{s} INSERT INTO t VALUES(1,2) RETURNING count(*)"),
        &format!("{s} INSERT INTO t VALUES(1,2) RETURNING rank() OVER ()"),
        &format!("{s} INSERT INTO t VALUES(1,2) RETURNING 1+count(*)"),
        // Plain (non-aggregate) RETURNING still runs, empty and non-empty.
        &format!("{s} DELETE FROM t RETURNING a"),
        &format!("{s} INSERT INTO t VALUES(5,6) RETURNING a, b"),
        &format!("{s} INSERT INTO t VALUES(5,6) RETURNING a+b AS s"),
        &format!("{s} INSERT INTO t VALUES(1,2); UPDATE t SET a=2 RETURNING a, b"),
        &format!("{s} INSERT INTO t VALUES(1,2); DELETE FROM t RETURNING a"),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
