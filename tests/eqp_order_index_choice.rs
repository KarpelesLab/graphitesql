//! No-`WHERE` `ORDER BY` index choice among several candidate indexes.
//!
//! When more than one index can satisfy a leading prefix of the `ORDER BY`, SQLite
//! reads from the one that orders the MOST terms (avoiding the largest sort), then
//! prefers a covering index, then the narrower / newer one. graphite's
//! `order_index_scan` used to return the first qualifying index in creation order,
//! so `SELECT a, b FROM t ORDER BY a, b` over `ia(a)` + `iab(a, b)` walked `ia` and
//! sorted `b` where sqlite reads the covering `iab` fully in order. It now scores
//! candidates and keeps the best. Verified byte-for-byte (plan and rows) against the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, base: &str, sql: &str) -> String {
    let o = Command::new(bin)
        .arg(":memory:")
        .arg(format!("{base}{sql}"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn plan(bin: &str, base: &str, q: &str) -> String {
    out(bin, base, &format!("EXPLAIN QUERY PLAN {q};"))
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("QUERY PLAN"))
        .map(|l| {
            l.trim_start_matches(|c: char| " |`*+_-".contains(c))
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("#")
}

#[test]
fn order_index_choice_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "CREATE TABLE t(id INTEGER PRIMARY KEY,a INT,b INT,c INT);\
                CREATE INDEX ia ON t(a);CREATE INDEX iab ON t(a,b);\
                CREATE INDEX iabc ON t(a,b,c);CREATE INDEX ibc ON t(b,c);\
                INSERT INTO t VALUES(1,1,3,1),(2,1,1,2),(3,2,2,3),(4,2,1,1),(5,3,5,2);";

    let cases: &[&str] = &[
        "SELECT a, b FROM t ORDER BY a, b",
        "SELECT a FROM t ORDER BY a",
        "SELECT a, b, c FROM t ORDER BY a, b, c",
        "SELECT a, b FROM t ORDER BY a",
        "SELECT b, c FROM t ORDER BY b, c",
        "SELECT a FROM t ORDER BY a DESC",
        "SELECT a, b FROM t ORDER BY a DESC, b DESC",
        "SELECT * FROM t ORDER BY a",
        "SELECT id, a FROM t ORDER BY a, id",
    ];
    for q in cases {
        assert_eq!(plan("sqlite3", base, q), plan(g, base, q), "plan for `{q}`");
        assert_eq!(out("sqlite3", base, q), out(g, base, q), "rows for `{q}`");
    }
}
