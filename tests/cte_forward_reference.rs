//! Within a single `WITH` clause, SQLite makes every CTE visible to its
//! siblings — *forward* references included — and rejects a true cycle with
//! `circular reference: <name>`, naming the CTE the outer query enters through
//! (it expands CTEs on demand). graphite previously exposed only the CTEs
//! declared *before* each one, so a forward reference fell through to the
//! schema and reported `no such table: <name>`.
//!
//! This checks both the now-supported forward references and the cycle naming:
//!   * `WITH a AS (SELECT * FROM b), b AS (SELECT 9) SELECT * FROM a` → `9`;
//!   * an `a`<->`b` cycle reports `a` from `… FROM a` but `b` from `… FROM b`;
//!   * a direct self-reference without `RECURSIVE` keeps its existing handling.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    let mut lines = Vec::new();
    for line in s.lines() {
        let mut t = line.trim_end();
        if t.trim_start().starts_with('^') {
            continue;
        }
        for prefix in [
            "Error: ",
            "in prepare, ",
            "stepping, ",
            "SQL error: ",
            "error: ",
        ] {
            t = t.strip_prefix(prefix).unwrap_or(t);
        }
        if let Some(rest) = t.strip_prefix("Parse error") {
            t = rest.split_once(": ").map_or(rest, |(_, m)| m);
        }
        if let Some(rest) = t.strip_prefix("Runtime error") {
            t = rest.split_once(": ").map_or(rest, |(_, m)| m);
        }
        lines.push(t.to_string());
    }
    lines.join("\n")
}

#[test]
fn cte_forward_reference_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Forward references now resolve.
        "WITH a AS (SELECT * FROM b), b AS (SELECT 9) SELECT * FROM a",
        "WITH a AS (SELECT * FROM c), b AS (SELECT 1), c AS (SELECT * FROM b) SELECT * FROM a",
        "WITH x AS (SELECT * FROM y), y AS (SELECT 5), z AS (SELECT * FROM x) SELECT * FROM z",
        // Cycle: named by the outer query's entry point.
        "WITH a AS (SELECT * FROM b), b AS (SELECT * FROM a) SELECT * FROM a",
        "WITH a AS (SELECT * FROM b), b AS (SELECT * FROM a) SELECT * FROM b",
        "WITH a AS (SELECT * FROM b), b AS (SELECT * FROM c), c AS (SELECT * FROM a) SELECT * FROM a",
        "WITH RECURSIVE a AS (SELECT * FROM b), b AS (SELECT * FROM a) SELECT * FROM a",
        // Forward reference through an INSERT ... WITH source.
        "CREATE TABLE t(n); INSERT INTO t WITH a AS (SELECT * FROM b), b AS (SELECT 7) \
         SELECT * FROM a; SELECT * FROM t",
        // Existing behaviour unchanged: ordinary recursion, backward references,
        // duplicate names, plain self-reference.
        "WITH RECURSIVE a(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM a) SELECT x FROM a LIMIT 3",
        "WITH b AS (SELECT 9), a AS (SELECT * FROM b) SELECT * FROM a",
        "WITH a AS (SELECT 1), a AS (SELECT 2) SELECT * FROM a",
        "WITH a AS (SELECT * FROM a) SELECT * FROM a",
        "WITH a AS (SELECT 1 UNION ALL SELECT 2) SELECT count(*) FROM a",
    ];
    for sql in cases {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
