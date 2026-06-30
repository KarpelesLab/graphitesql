//! `EXPLAIN QUERY PLAN` over a compound query (`… UNION / UNION ALL / INTERSECT
//! / EXCEPT …`). SQLite renders a `COMPOUND QUERY` node whose first child is the
//! `LEFT-MOST SUBQUERY` (the first arm's plan) followed by one operator node per
//! continuation — `UNION USING TEMP B-TREE`, `UNION ALL`, `INTERSECT USING TEMP
//! B-TREE`, `EXCEPT USING TEMP B-TREE` — each parenting that arm's plan. graphite
//! previously ignored the compound tail entirely and rendered only the first
//! arm's `SCAN`. A trailing `ORDER BY` on the whole compound switches SQLite to a
//! different `MERGE` plan — rendered for plain positional terms that cover all
//! output columns (see `tests/eqp_merge_compound.rs`); a partial cover, an
//! explicit `COLLATE`, a named term, or a `*` projection still decline here. A
//! bare `LIMIT`/`OFFSET` (no `ORDER BY`) keeps the plain `COMPOUND QUERY` tree.
//!
//! A multi-row `VALUES (…),(…),…` clause desugars internally to `UNION ALL` arms,
//! but SQLite folds them into a single `SCAN N-ROW VALUES CLAUSE` node (a one-row
//! `VALUES` is `SCAN CONSTANT ROW`). When such a clause is the left-most arm of —
//! or an operand within — a *real* compound, only its own rows fold; the genuine
//! `UNION`/`UNION ALL`/… boundaries still render as operator nodes. A row carrying
//! a subquery switches SQLite to a plural `SCAN N CONSTANT ROWS` shape with
//! interposed subquery nodes we don't model, so that declines cleanly. Verified vs
//! the sqlite3 3.50.4 CLI.

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

const BASE: &str = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a); CREATE TABLE u(x,y);";

#[test]
fn compound_query_tree_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM t UNION SELECT * FROM u",
        "SELECT * FROM t UNION ALL SELECT * FROM u",
        "SELECT a FROM t EXCEPT SELECT x FROM u",
        "SELECT a FROM t INTERSECT SELECT x FROM u",
        "SELECT 1 UNION SELECT 2",
        // A bare LIMIT/OFFSET keeps the plain COMPOUND QUERY tree.
        "SELECT * FROM t UNION SELECT * FROM u LIMIT 5",
        "SELECT * FROM t UNION ALL SELECT * FROM u LIMIT 5 OFFSET 2",
        // Three arms: LEFT-MOST plus two operator nodes.
        "SELECT * FROM t UNION SELECT * FROM u UNION SELECT 9, 9",
        // A per-arm WHERE seek and a shared WITH clause both carry through.
        "SELECT * FROM t WHERE a=5 UNION SELECT * FROM u WHERE x>3",
        "WITH c AS (SELECT 1) SELECT * FROM c UNION SELECT 2",
    ] {
        let sql = format!("{BASE} EXPLAIN QUERY PLAN {q}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {q}");
    }
}

#[test]
fn values_clause_renders_single_node() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        // A pure multi-row VALUES folds to one `SCAN N-ROW VALUES CLAUSE` node;
        // a single-row VALUES stays `SCAN CONSTANT ROW`.
        "VALUES(1,2)",
        "VALUES(1,2),(3,4)",
        "VALUES(1),(2),(3),(4),(5)",
        "VALUES(1+1,2),(3,4)",
        // A separate VALUES core joined by a real compound: each clause folds on
        // its own, the genuine boundary renders as an operator node.
        "VALUES(1,2) UNION ALL VALUES(3,4)",
        "VALUES(1,2) UNION ALL VALUES(3,4),(5,6)",
        "VALUES(1,2),(3,4) UNION VALUES(5,6)",
        "VALUES(1,2),(3,4) UNION ALL SELECT 5,6",
        // A hand-written SELECT-union (even aliased `columnN`) is NOT a VALUES
        // clause and keeps the per-arm `SCAN CONSTANT ROW` tree.
        "SELECT 1 AS column1 UNION ALL SELECT 2 AS column1",
    ] {
        let sql = format!("EXPLAIN QUERY PLAN {q}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {q}");
        // The executed rows must agree too.
        assert_eq!(run("sqlite3", q), run(g, q), "rows for {q}");
    }
}

#[test]
fn values_clause_with_subquery_declines() {
    // A subquery inside a VALUES row makes sqlite render `SCAN N CONSTANT ROWS`
    // plus a `SCALAR SUBQUERY` node — graphite declines rather than mis-render.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in ["VALUES((SELECT 1),2),(3,4)", "VALUES(1,2),((SELECT 3),4)"] {
        let sql = format!("EXPLAIN QUERY PLAN {q}");
        let got = run(g, &sql);
        assert!(
            got.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{q} should decline as unsupported, got {got:?}"
        );
    }
}

#[test]
fn order_by_on_a_compound_partial_or_fragile_declines() {
    // A trailing ORDER BY makes sqlite emit a `MERGE (<OP>)` tree. graphite renders
    // it for positional or named terms covering all output columns (see
    // `tests/eqp_merge_compound.rs`); the shapes below still decline cleanly rather
    // than mis-render — a `*` projection (output column count unresolved), a partial
    // cover (the merge appends a per-arm temp-b-tree), an explicit `COLLATE` (sqlite
    // uses a CO-ROUTINE+materialize plan), and a non-column expression term.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM t UNION SELECT * FROM u ORDER BY 1",
        "SELECT a, b FROM t EXCEPT SELECT x, y FROM u ORDER BY 1",
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY 1 COLLATE NOCASE",
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY a+0",
    ] {
        let sql = format!("{BASE} EXPLAIN QUERY PLAN {q}");
        let got = run(g, &sql);
        assert!(
            got.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{q} should decline as unsupported, got {got:?}"
        );
    }
}
