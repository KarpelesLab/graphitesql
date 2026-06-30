//! `EXPLAIN QUERY PLAN` over a compound (`UNION` / `INTERSECT` / `EXCEPT`) body
//! used as a derived-table `FROM` source or a CTE reference.
//!
//! A dedup set operator cannot be flattened into the outer plan, so SQLite
//! materializes the body as a `CO-ROUTINE <name>` whose single child is the
//! body's `COMPOUND QUERY` subtree (`LEFT-MOST SUBQUERY` plus one operator node
//! per arm — including any interspersed `UNION ALL`), followed by the outer
//! query's `{SCAN|SEARCH} <name>` and at most one trailing
//! `USE TEMP B-TREE FOR ORDER BY|GROUP BY|DISTINCT`. The label is the derived
//! table's alias or the CTE name.
//!
//! Graphite declines (rather than mis-render) the codegen-fragile shapes: an
//! unaliased derived table (SQLite numbers it `(subquery-N)`), a body whose every
//! operator is `UNION ALL` (it streams without a dedup b-tree and flattens to a
//! bare `COMPOUND QUERY`), and an outer `WHERE` (the predicate pushes into the
//! arms, re-deriving their scans). Verified vs the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run `EXPLAIN QUERY PLAN sql` and normalise the tree to a `#`-joined line of
/// bare node labels (drop the `QUERY PLAN` header and the box-drawing prefix),
/// matching the strip pipeline used elsewhere in the EQP tests.
fn plan(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} EXPLAIN QUERY PLAN {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|c: char| " |`*+_-".contains(c)))
        .collect::<Vec<_>>()
        .join("#")
}

/// Raw stderr/stdout for a decline case (graphite emits an `Error:` line).
fn raw(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} EXPLAIN QUERY PLAN {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        return stdout.trim_end().to_string();
    }
    String::from_utf8_lossy(&out.stderr).trim_end().to_string()
}

const BASE: &str = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a); CREATE TABLE u(x,y);";
const DATA: &str = "CREATE TABLE t(a,b); CREATE TABLE u(x,y); INSERT INTO t VALUES(1,2),(3,4); \
     INSERT INTO u VALUES(3,9),(5,7);";

#[test]
fn compound_body_renders_coroutine() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        // A dedup-operator body as an aliased derived source / CTE reference
        // materializes as CO-ROUTINE <label> over the body's COMPOUND QUERY.
        "SELECT * FROM (SELECT a FROM t UNION SELECT x FROM u) s",
        "SELECT * FROM (SELECT a FROM t UNION SELECT x FROM u) AS s",
        "SELECT * FROM (SELECT a FROM t INTERSECT SELECT x FROM u) s",
        "SELECT * FROM (SELECT a FROM t EXCEPT SELECT x FROM u) s",
        "WITH c AS (SELECT a FROM t UNION SELECT x FROM u) SELECT * FROM c",
        "WITH c AS (SELECT a FROM t INTERSECT SELECT x FROM u) SELECT * FROM c",
        "WITH c AS (SELECT a FROM t EXCEPT SELECT x FROM u) SELECT * FROM c",
        // One outer ORDER BY / GROUP BY / DISTINCT appends a temp-b-tree; a lone
        // min/max makes the outer access a SEARCH; count(*) keeps the SCAN.
        "SELECT * FROM (SELECT a FROM t UNION SELECT x FROM u) s ORDER BY 1",
        "SELECT DISTINCT * FROM (SELECT a FROM t UNION SELECT x FROM u) s",
        "SELECT * FROM (SELECT a FROM t UNION SELECT x FROM u) s GROUP BY a",
        "SELECT max(a) FROM (SELECT a FROM t UNION SELECT x FROM u) s",
        "SELECT count(*) FROM (SELECT a FROM t UNION SELECT x FROM u) s",
        "WITH c AS (SELECT a FROM t UNION SELECT x FROM u) SELECT DISTINCT a FROM c",
        "WITH c AS (SELECT a FROM t INTERSECT SELECT x FROM u) SELECT count(*) FROM c",
        // Three arms; a UNION ALL interspersed with a dedup operator still
        // materializes (the recursion renders the UNION ALL node verbatim).
        "SELECT * FROM (SELECT a FROM t UNION SELECT x FROM u UNION SELECT a FROM t) s",
        "WITH c AS (SELECT a FROM t UNION SELECT x FROM u UNION ALL SELECT a FROM t) SELECT * FROM c",
        "SELECT * FROM (SELECT a FROM t UNION ALL SELECT x FROM u UNION SELECT a FROM t) s",
        // A body LIMIT adds no node; a multi-column projection drops the covering
        // index naturally via the recursion.
        "SELECT * FROM (SELECT a FROM t UNION SELECT x FROM u LIMIT 3) s",
        "SELECT * FROM (SELECT a,b FROM t UNION SELECT x,y FROM u) s",
    ] {
        assert_eq!(plan("sqlite3", BASE, q), plan(g, BASE, q), "for {q}");
    }
}

#[test]
fn compound_coroutine_does_not_change_rows() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM (SELECT a FROM t UNION SELECT x FROM u) s ORDER BY 1",
        "WITH c AS (SELECT a FROM t INTERSECT SELECT x FROM u) SELECT * FROM c",
        "SELECT count(*) FROM (SELECT a FROM t UNION SELECT x FROM u) s",
        "SELECT * FROM (SELECT a FROM t EXCEPT SELECT x FROM u) s ORDER BY 1",
    ] {
        let sql = format!("{DATA} {q}");
        let a = Command::new("sqlite3")
            .arg(":memory:")
            .arg(&sql)
            .output()
            .unwrap();
        let b = Command::new(g).arg(":memory:").arg(&sql).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&a.stdout).trim_end(),
            String::from_utf8_lossy(&b.stdout).trim_end(),
            "rows for {q}"
        );
    }
}

#[test]
fn compound_body_fragile_shapes_decline() {
    // The codegen-fragile shapes graphite declines rather than mis-render:
    // an unaliased derived table (SQLite numbers it `(subquery-N)`), a
    // UNION-ALL-only body (flattens to a bare COMPOUND QUERY), and an outer
    // WHERE (the predicate pushes into the arms).
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM (SELECT a FROM t UNION SELECT x FROM u)",
        "SELECT * FROM (SELECT a FROM t UNION ALL SELECT x FROM u) s",
        "SELECT * FROM (SELECT a FROM t UNION SELECT x FROM u) s WHERE a>0",
        "WITH c AS (SELECT a FROM t UNION SELECT x FROM u) SELECT * FROM c WHERE a>0",
    ] {
        let got = raw(g, BASE, q);
        assert!(
            got.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{q} should decline as unsupported, got {got:?}"
        );
    }
}
