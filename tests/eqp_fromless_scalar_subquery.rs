//! `EXPLAIN QUERY PLAN` for a `FROM`-less SELECT carrying a non-correlated scalar
//! subquery in its projection or `WHERE`. SQLite renders the `SCAN CONSTANT ROW`
//! for the synthetic single row followed by a `SCALAR SUBQUERY N` sibling per
//! subquery (numbered left-to-right, body recursed as the child). graphite
//! previously emitted nothing for any subquery-bearing `FROM`-less SELECT; it now
//! renders the clean single-position cases.
//!
//! A cross-position set (subqueries in both the projection and the `WHERE`) is
//! reverse/renumbered, and an `EXISTS` / `IN (SELECT)` is a different node shape —
//! those still emit nothing (decline) rather than mis-render. Verified vs the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// `EXPLAIN QUERY PLAN sql` → `#`-joined bare node labels.
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

const BASE: &str = "CREATE TABLE u(x,y); CREATE INDEX ux ON u(x);";

#[test]
fn fromless_scalar_subquery_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT (SELECT count(*) FROM u) AS n",
        "SELECT (SELECT count(*) FROM u), (SELECT max(y) FROM u)",
        "SELECT 1, (SELECT count(*) FROM u)",
        "SELECT (SELECT 5)",
        "SELECT 1 WHERE (SELECT count(*) FROM u)>0",
        // No subquery — the plain constant row, unchanged.
        "SELECT 1",
        "SELECT 1+1 AS s",
    ] {
        assert_eq!(plan("sqlite3", BASE, q), plan(g, BASE, q), "plan for {q}");
    }
}

#[test]
fn fromless_fragile_subqueries_emit_nothing() {
    // A cross-position set (projection + WHERE) is reverse-numbered, an `EXISTS`
    // renders as a SCALAR SUBQUERY only in some positions, and `IN (SELECT)` is a
    // LIST SUBQUERY + bloom filter — graphite declines (emits no plan node) rather
    // than mis-render. We assert it produces no `SCALAR SUBQUERY` node.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT (SELECT count(*) FROM u) WHERE (SELECT max(y) FROM u)>0",
        "SELECT (SELECT x FROM u WHERE x IN (SELECT y FROM u))",
        "SELECT EXISTS(SELECT 1 FROM u)",
    ] {
        let got = plan(g, BASE, q);
        assert!(
            !got.contains("SCALAR SUBQUERY"),
            "{q} should decline (no scalar node), got {got:?}"
        );
    }
}
