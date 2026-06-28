//! Window `PARTITION BY` / `ORDER BY` honor the key's collation — an explicit
//! `COLLATE` operator or the column's declared collation — when partitioning
//! rows, ordering them, and grouping order-by peers (for `rank`, `dense_rank`,
//! `percent_rank`, `cume_dist`, and `RANGE`/`GROUPS` frames). graphite compared
//! window keys under `BINARY` regardless, so `PARTITION BY g COLLATE NOCASE`
//! split `'a'` and `'A'` into separate partitions and `rank() OVER (ORDER BY g
//! COLLATE NOCASE)` treated them as distinct peers — both producing silently
//! wrong values.
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
fn window_keys_honor_collation() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // PARTITION BY with explicit COLLATE NOCASE: 'B' and 'b' share a partition.
        "CREATE TABLE t(x,g); INSERT INTO t VALUES(5,'B'),(3,'a'),(8,'b'); \
         SELECT x,g, sum(x) OVER (PARTITION BY g COLLATE NOCASE ORDER BY x) FROM t \
         ORDER BY x",
        // ORDER BY COLLATE NOCASE: 'B' and 'b' are peers, so the running sum jumps.
        "CREATE TABLE t(x,g); INSERT INTO t VALUES(5,'B'),(3,'a'),(8,'b'); \
         SELECT x,g, sum(x) OVER (ORDER BY g COLLATE NOCASE) FROM t ORDER BY g COLLATE NOCASE, x",
        // rank / dense_rank peer-group under NOCASE.
        "CREATE TABLE t(g); INSERT INTO t VALUES('a'),('A'),('b'),('B'); \
         SELECT g, rank() OVER (ORDER BY g COLLATE NOCASE), \
         dense_rank() OVER (ORDER BY g COLLATE NOCASE) FROM t ORDER BY g COLLATE NOCASE, g",
        // percent_rank / cume_dist peer-group under NOCASE.
        "CREATE TABLE t(g); INSERT INTO t VALUES('a'),('A'),('b'); \
         SELECT g, percent_rank() OVER (ORDER BY g COLLATE NOCASE), \
         cume_dist() OVER (ORDER BY g COLLATE NOCASE) FROM t ORDER BY g COLLATE NOCASE, g",
        // Declared column collation (no explicit COLLATE in the window).
        "CREATE TABLE t(g COLLATE NOCASE, x); INSERT INTO t VALUES('a',1),('A',2),('b',3); \
         SELECT g, x, count(*) OVER (PARTITION BY g) FROM t ORDER BY g, x",
        // RANGE default frame peers under NOCASE.
        "CREATE TABLE t(g,x); INSERT INTO t VALUES('a',1),('A',2),('b',3); \
         SELECT g, sum(x) OVER (ORDER BY g COLLATE NOCASE \
         RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY g COLLATE NOCASE, x",
        // Control: BINARY (default) keeps 'a' and 'A' distinct.
        "CREATE TABLE t(g); INSERT INTO t VALUES('a'),('A'),('b'),('B'); \
         SELECT g, rank() OVER (ORDER BY g), count(*) OVER (PARTITION BY g) FROM t ORDER BY g",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
