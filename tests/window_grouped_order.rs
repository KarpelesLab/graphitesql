//! A `GROUP BY` query with a window function and no outer `ORDER BY` emits its
//! grouped rows in the *first* window's `(PARTITION BY …, ORDER BY …)` order —
//! exactly like SQLite, and exactly as a plain (non-grouped) window query does
//! (see `window_output_order.rs`). The window runs over the grouped rows, so its
//! `ORDER BY` may reference an aggregate of the group (`sum(x)`, `avg(x)`).
//! graphite previously left the grouped rows in group-key order, so
//! `SELECT g, sum(x), row_number() OVER (ORDER BY sum(x) DESC) FROM t GROUP BY g`
//! came out ascending-by-`sum` instead of descending.
//!
//! `OVER ()` (no partition, no order) induces no ordering and the group-key
//! order is left untouched.
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
fn grouped_window_query_emits_rows_in_window_order() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // ORDER BY an aggregate, descending — the divergent case.
        "CREATE TABLE t(x,g); INSERT INTO t VALUES(5,'b'),(3,'a'),(8,'b'); \
         SELECT g, sum(x), row_number() OVER (ORDER BY sum(x) DESC) FROM t GROUP BY g",
        // ORDER BY an aggregate, ascending.
        "CREATE TABLE t(x,g); INSERT INTO t VALUES(5,'b'),(3,'a'),(8,'c'); \
         SELECT g, sum(x), rank() OVER (ORDER BY sum(x)) FROM t GROUP BY g",
        // avg() as the window key.
        "CREATE TABLE t(x,g); INSERT INTO t VALUES(5,'b'),(3,'a'),(8,'c'); \
         SELECT g, avg(x), row_number() OVER (ORDER BY avg(x)) FROM t GROUP BY g",
        // PARTITION BY an aggregate-free group key, ORDER BY an aggregate.
        "CREATE TABLE t(x,g,h); INSERT INTO t VALUES(5,'b',1),(3,'a',1),(8,'b',2); \
         SELECT g, sum(x), dense_rank() OVER (PARTITION BY g ORDER BY sum(x)) \
         FROM t GROUP BY g,h",
        // count(*) OVER () induces no ordering — group-key order is preserved.
        "CREATE TABLE t(x,g); INSERT INTO t VALUES(5,'b'),(3,'a'),(8,'b'); \
         SELECT g, sum(x), count(*) OVER () FROM t GROUP BY g",
        // A named window referencing an aggregate.
        "CREATE TABLE t(x,g); INSERT INTO t VALUES(5,'b'),(3,'a'),(8,'c'); \
         SELECT g, sum(x), row_number() OVER w FROM t GROUP BY g \
         WINDOW w AS (ORDER BY sum(x) DESC)",
        // An explicit outer ORDER BY still overrides the window order.
        "CREATE TABLE t(x,g); INSERT INTO t VALUES(5,'b'),(3,'a'),(8,'b'); \
         SELECT g, sum(x), row_number() OVER (ORDER BY sum(x) DESC) \
         FROM t GROUP BY g ORDER BY g",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
