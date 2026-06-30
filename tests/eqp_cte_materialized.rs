//! `EXPLAIN QUERY PLAN` over a CTE carrying an explicit `MATERIALIZED` hint.
//! `WITH c AS MATERIALIZED (…)` forces SQLite to materialize the CTE instead of
//! flattening its body into the outer plan, rendering a `MATERIALIZE <name>` node
//! whose child is the body's plan (recursed normally — `SCAN t`, an index scan, a
//! `SCAN {N} CONSTANT ROWS` for a multi-row `VALUES` body, etc.), followed by the
//! outer query's `{SCAN|SEARCH} <name>` plus at most one trailing
//! `USE TEMP B-TREE FOR ORDER BY|GROUP BY|DISTINCT`. The hint never changes the
//! executed rows (graphite materializes lazily either way) — only the plan. A
//! `NOT MATERIALIZED` (or absent) hint keeps the existing flatten behaviour. A
//! `VALUES` body whose row carries a subquery (SQLite interposes a
//! `SCALAR SUBQUERY` node) and a combination of outer clauses both decline cleanly
//! rather than mis-render. Verified vs the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        return stdout.trim_end().to_string();
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

const BASE: &str = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a);";
const DATA: &str = "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(3,4),(5,6);";

#[test]
fn materialized_cte_renders_materialize_node() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        // A MATERIALIZED hint forces the MATERIALIZE node even for a body that
        // would otherwise flatten into the outer SCAN.
        "WITH c AS MATERIALIZED (SELECT * FROM t) SELECT * FROM c",
        "WITH c AS MATERIALIZED (SELECT * FROM t) SELECT * FROM c WHERE a=1",
        "WITH c AS MATERIALIZED (SELECT a FROM t WHERE b>1) SELECT * FROM c",
        "WITH c AS MATERIALIZED (SELECT a FROM t WHERE a=5) SELECT * FROM c",
        "WITH c AS MATERIALIZED (SELECT * FROM t) SELECT count(*) FROM c",
        // One outer ORDER BY / GROUP BY / DISTINCT appends a temp-b-tree; a lone
        // min/max makes the outer access a SEARCH (child stays SCAN).
        "WITH c AS MATERIALIZED (SELECT * FROM t) SELECT * FROM c ORDER BY a",
        "WITH c AS MATERIALIZED (SELECT * FROM t) SELECT * FROM c GROUP BY a",
        "WITH c AS MATERIALIZED (SELECT * FROM t) SELECT DISTINCT a FROM c",
        "WITH c AS MATERIALIZED (SELECT * FROM t) SELECT max(a) FROM c",
        // A multi-row VALUES body materializes as SCAN N CONSTANT ROWS; a
        // single-row body is the singular SCAN CONSTANT ROW.
        "WITH c AS MATERIALIZED (VALUES(1,2),(3,4)) SELECT * FROM c",
        "WITH c AS MATERIALIZED (VALUES(1)) SELECT * FROM c",
        // NOT MATERIALIZED / absent hint keeps the flatten behaviour.
        "WITH c AS NOT MATERIALIZED (SELECT * FROM t) SELECT * FROM c",
        "WITH c AS (SELECT * FROM t) SELECT * FROM c",
    ] {
        let sql = format!("{BASE} EXPLAIN QUERY PLAN {q}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {q}");
    }
}

#[test]
fn materialized_hint_does_not_change_rows() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "WITH c AS MATERIALIZED (SELECT * FROM t) SELECT * FROM c",
        "WITH c AS MATERIALIZED (SELECT a FROM t WHERE b>2) SELECT sum(a) FROM c",
        "WITH c AS MATERIALIZED (VALUES(1,2),(3,4)) SELECT * FROM c",
        "WITH c AS NOT MATERIALIZED (SELECT * FROM t) SELECT max(a) FROM c",
    ] {
        let sql = format!("{DATA} {q}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "rows for {q}");
    }
}

#[test]
fn materialized_values_subquery_body_declines() {
    // A VALUES body whose row carries a subquery makes SQLite interpose a
    // SCALAR SUBQUERY node we don't model — graphite declines rather than emit a
    // MATERIALIZE node missing its child.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "WITH c AS MATERIALIZED (VALUES((SELECT 1))) SELECT * FROM c",
        "WITH c AS MATERIALIZED (VALUES((SELECT 1),2),(3,4)) SELECT * FROM c",
    ] {
        let sql = format!("EXPLAIN QUERY PLAN {q}");
        let got = run(g, &sql);
        assert!(
            got.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{q} should decline as unsupported, got {got:?}"
        );
    }
}
