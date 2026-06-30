//! `EXPLAIN QUERY PLAN` for an UPDATE/DELETE/INSERT carrying a non-correlated
//! scalar subquery — in a `SET` assignment, the `WHERE` clause, or a single-row
//! `VALUES`. SQLite renders the access node (none for an INSERT) followed by a
//! `SCALAR SUBQUERY N` sibling whose child is the subquery body's plan — exactly as
//! it does for a SELECT. graphite previously emitted only the access node (nothing
//! for an INSERT), diverging on every such statement; it now renders the node for
//! the unambiguous single-subquery case (always `SCALAR SUBQUERY 1`).
//!
//! Several `SET` subqueries are emitted in source order but numbered in *reverse*
//! (codegen-fragile), and a correlated body becomes a `CORRELATED SCALAR SUBQUERY`
//! / an `IN (SELECT)` a `LIST SUBQUERY` + bloom filter — different shapes graphite
//! does not model — so those decline (keep the bare access node) rather than
//! mis-render. Verified vs the sqlite3 3.50.4 CLI.

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

const BASE: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); \
                    CREATE INDEX tb ON t(b); CREATE TABLE u(x,y);";

#[test]
fn single_scalar_subquery_renders_like_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        // Subquery in a SET assignment, with and without a WHERE.
        "UPDATE t SET c=(SELECT count(*) FROM u) WHERE b=1",
        "UPDATE t SET c=(SELECT count(*) FROM u)",
        "UPDATE t SET c=(SELECT 5)",
        "UPDATE t SET c=(SELECT max(y) FROM u) WHERE a=2",
        // Subquery in the WHERE clause (UPDATE and DELETE).
        "UPDATE t SET c=1 WHERE c=(SELECT count(*) FROM u)",
        "DELETE FROM t WHERE c=(SELECT count(*) FROM u)",
        "DELETE FROM t WHERE c=(SELECT 5)",
        // A single-row INSERT VALUES carrying one scalar subquery (no access node
        // of its own — the scalar node is the whole plan).
        "INSERT INTO t(b) VALUES((SELECT count(*) FROM u))",
        "INSERT INTO t VALUES(1,(SELECT max(y) FROM u),3)",
        "INSERT INTO t(b) VALUES((SELECT 5))",
    ] {
        assert_eq!(plan("sqlite3", BASE, q), plan(g, BASE, q), "plan for {q}");
        // The node must actually be present (guards against both rendering nothing).
        assert!(
            plan(g, BASE, q).contains("SCALAR SUBQUERY 1"),
            "expected a SCALAR SUBQUERY 1 node for {q}, got {:?}",
            plan(g, BASE, q)
        );
    }
}

#[test]
fn fragile_or_correlated_subqueries_decline_to_the_bare_access_node() {
    // These keep the bare access node (graphite emits no scalar node) rather than
    // mis-render: multiple SET subqueries are reverse-numbered, a SET+WHERE pair
    // shares the same fragile counter, a correlated body is a different node kind,
    // and `IN (SELECT)` is a LIST SUBQUERY + bloom filter. Asserted by confirming
    // graphite emits no `SCALAR SUBQUERY` node at all for them.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "UPDATE t SET c=(SELECT max(y) FROM u), b=(SELECT min(x) FROM u) WHERE a=2",
        "UPDATE t SET c=(SELECT count(*) FROM u) WHERE c=(SELECT max(x) FROM u)",
        "UPDATE t SET c=(SELECT y FROM u WHERE x=t.a)",
        "DELETE FROM t WHERE a IN (SELECT x FROM u)",
        // INSERT: multi-subquery (reverse-numbered), multi-row (+SCAN N CONSTANT
        // ROWS), and a nested IN (LIST SUBQUERY + bloom) all decline.
        "INSERT INTO t VALUES((SELECT min(x) FROM u),(SELECT max(y) FROM u),3)",
        "INSERT INTO t(b) VALUES((SELECT count(*) FROM u)),((SELECT max(y) FROM u))",
        "INSERT INTO t(b) VALUES((SELECT x FROM u WHERE x IN (SELECT y FROM u)))",
    ] {
        let got = plan(g, BASE, q);
        assert!(
            !got.contains("SCALAR SUBQUERY"),
            "{q} should decline (no scalar node), got {got:?}"
        );
    }
}

#[test]
fn plain_dml_without_a_subquery_is_unchanged() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "UPDATE t SET c=1 WHERE b=5",
        "UPDATE t SET c=1 WHERE a=3",
        "UPDATE t SET c=c+1",
        "DELETE FROM t WHERE b>5",
        "DELETE FROM t WHERE a=3",
        "INSERT INTO t(b) VALUES(5)",
        "INSERT INTO t VALUES(1,2,3)",
    ] {
        assert_eq!(plan("sqlite3", BASE, q), plan(g, BASE, q), "plan for {q}");
    }
}
