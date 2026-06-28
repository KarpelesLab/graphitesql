//! A plain window-function `SELECT` with no outer `ORDER BY` emits its rows in
//! the *first* window's `(PARTITION BY …, ORDER BY …)` order, exactly like
//! SQLite. SQLite evaluates a window by sorting the rows into partition+order
//! order and never shuffles them back to the scan order, so the result set
//! comes out sorted even though the query named no `ORDER BY`. graphite
//! previously left the rows in table-scan order, so `SELECT x, row_number()
//! OVER (ORDER BY x) FROM t` over rows inserted `5,3,8` returned them `5,3,8`
//! instead of `3,5,8`.
//!
//! The *first* window in the select list wins (a second window with a different
//! `ORDER BY` does not change the row order); `OVER ()` (no partition, no order)
//! induces no ordering and the scan order is left untouched.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s.trim_end().to_string()
}

#[test]
fn window_query_emits_rows_in_window_order() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Each case builds a table whose insertion order is deliberately NOT the
    // window's order, so a row-for-row match proves graphite reordered like
    // sqlite rather than echoing the scan order.
    let cases = [
        // Single ORDER BY ascending / descending.
        "CREATE TABLE t(x); INSERT INTO t VALUES(5),(3),(8); \
         SELECT x, row_number() OVER (ORDER BY x) FROM t",
        "CREATE TABLE t(x); INSERT INTO t VALUES(5),(3),(8); \
         SELECT x, sum(x) OVER (ORDER BY x) FROM t",
        "CREATE TABLE t(x); INSERT INTO t VALUES(5),(3),(8); \
         SELECT x, sum(x) OVER (ORDER BY x DESC) FROM t",
        // PARTITION BY sorts the partitions ascending, then orders within each.
        "CREATE TABLE t(x,g); INSERT INTO t VALUES(5,'b'),(3,'a'),(8,'b'),(1,'a'); \
         SELECT x,g, sum(x) OVER (PARTITION BY g ORDER BY x) FROM t",
        // PARTITION BY with no ORDER BY: partitions ascending, scan order within.
        "CREATE TABLE t(x,g); INSERT INTO t VALUES(5,'b'),(3,'a'),(8,'b'); \
         SELECT x,g, sum(x) OVER (PARTITION BY g) FROM t",
        // The FIRST window in the select list determines the row order.
        "CREATE TABLE t(x,y); INSERT INTO t VALUES(5,20),(3,30),(8,10); \
         SELECT x,y, sum(x) OVER (ORDER BY x), sum(y) OVER (ORDER BY y) FROM t",
        "CREATE TABLE t(x,y); INSERT INTO t VALUES(5,20),(3,30),(8,10); \
         SELECT x,y, sum(y) OVER (ORDER BY y), sum(x) OVER (ORDER BY x) FROM t",
        // Named window via OVER w.
        "CREATE TABLE t(x); INSERT INTO t VALUES(5),(3),(8); \
         SELECT x, first_value(x) OVER w, last_value(x) OVER w FROM t \
         WINDOW w AS (ORDER BY x)",
        "CREATE TABLE t(x); INSERT INTO t VALUES(5),(3),(8); \
         SELECT x, lag(x) OVER w, lead(x) OVER w FROM t WINDOW w AS (ORDER BY x DESC)",
        // NULLs participate (sort first under ASC).
        "CREATE TABLE t(x); INSERT INTO t VALUES(NULL),(3),(NULL),(8); \
         SELECT x, count(*) OVER (ORDER BY x) FROM t",
        // OVER () induces NO ordering — scan order is preserved.
        "CREATE TABLE t(x); INSERT INTO t VALUES(5),(3),(8); \
         SELECT x, count(*) OVER () FROM t",
        // DISTINCT downstream preserves the window order.
        "CREATE TABLE t(x); INSERT INTO t VALUES(5),(3),(8),(3); \
         SELECT DISTINCT x, sum(x) OVER (ORDER BY x) FROM t",
        // An explicit ORDER BY still overrides the window order.
        "CREATE TABLE t(x); INSERT INTO t VALUES(5),(3),(8); \
         SELECT x, sum(x) OVER (ORDER BY x) FROM t ORDER BY x DESC",
        // LIMIT applies after the window-induced ordering.
        "CREATE TABLE t(x); INSERT INTO t VALUES(5),(3),(8); \
         SELECT x, sum(x) OVER (ORDER BY x) FROM t LIMIT 2",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
