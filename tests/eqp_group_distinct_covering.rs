//! A `GROUP BY` / `DISTINCT` over a covered column reads from a covering index to
//! produce its keys in order — and when *several* indexes cover the query, SQLite
//! picks the narrowest (ties → newest), exactly like a bare covering scan. graphite
//! used to decline whenever more than one index qualified (falling back to a plain
//! `SCAN t` + sort); `covering_scan` now picks deterministically. Verified
//! byte-for-byte (plan and rows) against the sqlite3 3.50.4 CLI.

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
fn group_distinct_covering_multi_index_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // `ia(a)` and `iab(a,b)` both cover a query over `a`; the narrower `ia` wins.
    let base = "CREATE TABLE t(id INTEGER PRIMARY KEY,a INT,b INT);\
                CREATE INDEX ia ON t(a);CREATE INDEX iab ON t(a,b);\
                INSERT INTO t VALUES(1,1,10),(2,1,20),(3,2,30),(4,2,10),(5,3,5);";
    // Same, opposite creation order — the pick must not depend on it.
    let base_rev = "CREATE TABLE t(id INTEGER PRIMARY KEY,a INT,b INT);\
                    CREATE INDEX iab ON t(a,b);CREATE INDEX ia ON t(a);\
                    INSERT INTO t VALUES(1,1,10),(2,1,20),(3,2,30);";

    let cases: &[(&str, &str)] = &[
        (base, "SELECT a FROM t GROUP BY a"),
        (base, "SELECT a, count(*) FROM t GROUP BY a"),
        (base, "SELECT a, sum(b) FROM t GROUP BY a"),
        (base, "SELECT DISTINCT a FROM t"),
        (base, "SELECT a FROM t GROUP BY a HAVING count(*) > 1"),
        (base, "SELECT DISTINCT a FROM t ORDER BY a DESC"),
        (base_rev, "SELECT a FROM t GROUP BY a"),
        (base_rev, "SELECT DISTINCT a FROM t"),
    ];
    for (b, q) in cases {
        assert_eq!(plan("sqlite3", b, q), plan(g, b, q), "plan for `{q}`");
        assert_eq!(out("sqlite3", b, q), out(g, b, q), "rows for `{q}`");
    }
}
