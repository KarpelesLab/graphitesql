//! Track B (EQP): a recursive CTE source. `WITH RECURSIVE c(…) AS (<anchor>
//! UNION[ ALL] <recursive>) SELECT … FROM c` is rendered by SQLite as a
//! `CO-ROUTINE c` node with two children — `SETUP` (the non-recursive anchor's
//! plan) and `RECURSIVE STEP` (the recursive arm's plan, in which the
//! self-reference reads as a plain `SCAN c` of the materialized table) — followed
//! by the outer `SCAN c`. graphite previously errored
//! (`EXPLAIN QUERY PLAN for this query shape`) on every such query; it now renders
//! the canonical two-arm shape byte-exactly.
//!
//! The rendered slice is the common one: exactly one anchor arm that does not name
//! the CTE, exactly one recursive arm whose `FROM` is a *bare* reference to it
//! (`FROM c`, no join/alias/subquery), and an outer query that is a single bare
//! reference adding no further plan node (an outer `WHERE` and a bare aggregate add
//! none). The outer access over the materialized co-routine is normally a `SCAN`,
//! except a lone `min()`/`max()` aggregate seeks one end and reads as `SEARCH c`
//! (no index detail — a co-routine has none); a second aggregate keeps the `SCAN`,
//! and a `min(DISTINCT …)` (which interposes a `USE TEMP B-TREE FOR min(DISTINCT)`
//! node) declines. The anchor arm itself is recursed normally, so a `SELECT <consts>` /
//! `VALUES(…)` body renders `SCAN CONSTANT ROW` and a `SELECT … FROM t` body renders
//! `SCAN t`. `UNION` vs `UNION ALL` is the same plan.
//!
//! A single outer `ORDER BY`, `GROUP BY`, or `DISTINCT` appends one root-level
//! `USE TEMP B-TREE FOR ORDER BY` / `GROUP BY` / `DISTINCT` node after the outer
//! scan, and is rendered (the min/max `SEARCH` access still applies independently,
//! so a `DISTINCT max(n)` is `SEARCH c` plus the DISTINCT sorter).
//!
//! Declined (graphite keeps its prior `Unsupported` error — never a wrong plan; the
//! executed rows always match): a join in the recursive arm (`FROM c, t` — an extra
//! scan child), a *combination* of outer ORDER BY / GROUP BY / DISTINCT (SQLite
//! folds or reorders the temp-b-tree nodes), a `min(DISTINCT …)` (interposes its own
//! `USE TEMP B-TREE FOR min(DISTINCT)` node), and any non-canonical arm split.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn g_eqp(ddl: &str, q: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let rows = c.query(&format!("EXPLAIN QUERY PLAN {q}")).unwrap().rows;
    let mut lines = Vec::new();
    for r in &rows {
        if let Some(graphitesql::Value::Text(s)) = r.last() {
            lines.push(String::from(s.as_str()));
        }
    }
    lines.join(" | ")
}

fn sqlite_eqp(ddl: &str, q: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{ddl} EXPLAIN QUERY PLAN {q};"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).trim_end())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn g_rows(ddl: &str, q: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let r = c.query(q).unwrap();
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    graphitesql::Value::Null => "".to_string(),
                    graphitesql::Value::Integer(i) => i.to_string(),
                    graphitesql::Value::Real(f) => format!("{f}"),
                    graphitesql::Value::Text(s) => String::from(s.as_str()),
                    graphitesql::Value::Blob(_) => "<blob>".to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Like `g_eqp` but tolerates a declined shape: graphite returns an
/// `Unsupported` error from `EXPLAIN QUERY PLAN`, which we map to an empty plan
/// (no node) rather than panicking.
fn g_eqp_opt(ddl: &str, q: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    match c.query(&format!("EXPLAIN QUERY PLAN {q}")) {
        Ok(res) => res
            .rows
            .iter()
            .filter_map(|r| match r.last() {
                Some(graphitesql::Value::Text(s)) => Some(String::from(s.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | "),
        Err(_) => String::new(),
    }
}

fn sqlite_rows(ddl: &str, q: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{ddl} {q};"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn check(ddl: &str, q: &str) {
    assert_eq!(g_eqp(ddl, q), sqlite_eqp(ddl, q), "EQP diverged for {q}");
    assert_eq!(g_rows(ddl, q), sqlite_rows(ddl, q), "rows diverged for {q}");
}

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

const D: &str = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);";

/// The canonical recursive CTE renders the `CO-ROUTINE`/`SETUP`/`RECURSIVE STEP`
/// subtree followed by the outer `SCAN` — byte-exact plan and rows.
#[test]
fn recursive_cte_renders_co_routine() {
    if !have_sqlite() {
        return;
    }
    assert_eq!(
        g_eqp(
            D,
            "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) SELECT * FROM c"
        ),
        "CO-ROUTINE c | SETUP | SCAN CONSTANT ROW | RECURSIVE STEP | SCAN c | SCAN c"
    );
    // An anchor that scans a base table renders `SCAN t` under SETUP.
    assert_eq!(
        g_eqp(
            D,
            "WITH RECURSIVE c(n) AS (SELECT a FROM t UNION ALL SELECT n+1 FROM c WHERE n<5) \
             SELECT * FROM c"
        ),
        "CO-ROUTINE c | SETUP | SCAN t | RECURSIVE STEP | SCAN c | SCAN c"
    );
    // A lone `min()`/`max()` over the co-routine seeks one end, so the outer
    // access reads `SEARCH c` (no index detail — a co-routine has none) rather
    // than `SCAN c`. Scalar wrappers, an expression argument, and an extra plain
    // column keep the single-aggregate shape.
    for q in [
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT max(n) FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT min(n) FROM c WHERE n>1",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT abs(min(n)) FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT min(n+1) FROM c",
    ] {
        assert_eq!(
            g_eqp(D, q),
            "CO-ROUTINE c | SETUP | SCAN CONSTANT ROW | RECURSIVE STEP | SCAN c | SEARCH c",
            "min/max outer should render SEARCH for {q}"
        );
        check(D, q);
    }
    // A *second* aggregate disqualifies the min/max optimization — the outer
    // access stays a plain `SCAN c`.
    for q in [
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT min(n),max(n) FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT min(n),count(*) FROM c",
    ] {
        assert_eq!(
            g_eqp(D, q),
            "CO-ROUTINE c | SETUP | SCAN CONSTANT ROW | RECURSIVE STEP | SCAN c | SCAN c",
            "two aggregates should keep SCAN for {q}"
        );
        check(D, q);
    }
    for q in [
        // A `SELECT <const>` anchor and a counter recursion (the textbook form).
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) SELECT * FROM c",
        // A narrower outer projection — still a bare `SCAN c`.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) SELECT n FROM c",
        // An outer `WHERE` adds no node (a co-routine source is never a SEARCH).
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT * FROM c WHERE n>2",
        // A bare aggregate over the CTE adds no grouping node.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT count(*) FROM c",
        // `UNION` (distinct) instead of `UNION ALL` — same plan.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION SELECT n+1 FROM c WHERE n<5) SELECT * FROM c",
        // A `VALUES(…)` anchor desugars to `SCAN CONSTANT ROW` like a const SELECT.
        "WITH RECURSIVE c(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM c WHERE n<5) SELECT * FROM c",
        // (The base-table-anchor plan — `SELECT a FROM t UNION ALL …` → `SCAN t`
        // under SETUP — is asserted above via `g_eqp`; its rows are not run here
        // because executing that recursion currently hits a separate, pre-existing
        // executor stack overflow tracked as its own fix.)
    ] {
        assert!(
            g_eqp(D, q).contains("CO-ROUTINE c"),
            "expected a CO-ROUTINE node for {q}, got {}",
            g_eqp(D, q)
        );
        check(D, q);
    }
}

/// Declined shapes keep graphite's prior `Unsupported` error — never a wrong plan —
/// while the executed rows still match SQLite.
#[test]
fn recursive_cte_declines_unrenderable() {
    if !have_sqlite() {
        return;
    }
    for q in [
        // A join in the recursive arm adds a second scan child under RECURSIVE STEP.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c, t WHERE n<5) SELECT * FROM c",
        // A *combination* of grouping clauses can fold or reorder the single
        // trailing sorter, so each combination declines (a lone ORDER BY / GROUP BY
        // / DISTINCT renders — see `recursive_cte_renders_trailing_temp_btree`).
        // GROUP BY + ORDER BY folds the ORDER BY into the grouping sorter.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT n FROM c GROUP BY n ORDER BY n DESC",
        // DISTINCT + ORDER BY likewise collapses to one node.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT DISTINCT n FROM c ORDER BY n",
        // A bare `min()`/`max()` with an ORDER BY: SQLite elides the sort (single
        // row) and reads `SEARCH c`, but our after-scan trailing logic only fires
        // for a non-aggregate ORDER BY, so this declines rather than render.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT max(n) FROM c ORDER BY 1",
        // A `min(DISTINCT …)` over the co-routine interposes a
        // `USE TEMP B-TREE FOR min(DISTINCT)` node before the `SEARCH c` that
        // graphite does not render, so it declines rather than emit a wrong plan.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT min(DISTINCT n) FROM c",
    ] {
        // We must never emit a CO-ROUTINE plan that disagrees with SQLite for a
        // declined shape; the prior behavior was an error, so assert no node.
        assert!(
            !g_eqp_opt(D, q).contains("CO-ROUTINE"),
            "expected no CO-ROUTINE node for the declined shape {q}, got {}",
            g_eqp_opt(D, q)
        );
        // The executed rows must still match regardless.
        assert_eq!(g_rows(D, q), sqlite_rows(D, q), "rows diverged for {q}");
    }
}

/// An outer `ORDER BY` / `GROUP BY` / `DISTINCT` over the co-routine appends a
/// single `USE TEMP B-TREE FOR …` node at the root level (after the outer scan).
/// The access keyword still follows the min/max optimization independently — a
/// `DISTINCT max(n)` is `SEARCH c` plus the DISTINCT sorter.
#[test]
fn recursive_cte_renders_trailing_temp_btree() {
    if !have_sqlite() {
        return;
    }
    const BASE: &str =
        "WITH RECURSIVE c(n,m) AS (SELECT 1,1 UNION ALL SELECT n+1,m*2 FROM c WHERE n<5) ";
    let cases = [
        // Outer ORDER BY over a non-aggregate projection → ORDER BY sorter.
        (
            "SELECT * FROM c ORDER BY n DESC",
            "CO-ROUTINE c | SETUP | SCAN CONSTANT ROW | RECURSIVE STEP | SCAN c | SCAN c | \
             USE TEMP B-TREE FOR ORDER BY",
        ),
        (
            "SELECT n,m FROM c WHERE n>2 ORDER BY m",
            "CO-ROUTINE c | SETUP | SCAN CONSTANT ROW | RECURSIVE STEP | SCAN c | SCAN c | \
             USE TEMP B-TREE FOR ORDER BY",
        ),
        // Outer GROUP BY (HAVING rides along) → GROUP BY sorter; a per-group
        // max() keeps the SCAN (the min/max seek does not apply with GROUP BY).
        (
            "SELECT n FROM c GROUP BY n",
            "CO-ROUTINE c | SETUP | SCAN CONSTANT ROW | RECURSIVE STEP | SCAN c | SCAN c | \
             USE TEMP B-TREE FOR GROUP BY",
        ),
        (
            "SELECT count(*),n FROM c GROUP BY n HAVING count(*)>0",
            "CO-ROUTINE c | SETUP | SCAN CONSTANT ROW | RECURSIVE STEP | SCAN c | SCAN c | \
             USE TEMP B-TREE FOR GROUP BY",
        ),
        (
            "SELECT max(n) FROM c GROUP BY m",
            "CO-ROUTINE c | SETUP | SCAN CONSTANT ROW | RECURSIVE STEP | SCAN c | SCAN c | \
             USE TEMP B-TREE FOR GROUP BY",
        ),
        // Statement-level DISTINCT → DISTINCT sorter (even over a single-row
        // aggregate, SQLite still emits it).
        (
            "SELECT DISTINCT n FROM c",
            "CO-ROUTINE c | SETUP | SCAN CONSTANT ROW | RECURSIVE STEP | SCAN c | SCAN c | \
             USE TEMP B-TREE FOR DISTINCT",
        ),
        (
            "SELECT DISTINCT count(*) FROM c",
            "CO-ROUTINE c | SETUP | SCAN CONSTANT ROW | RECURSIVE STEP | SCAN c | SCAN c | \
             USE TEMP B-TREE FOR DISTINCT",
        ),
        // DISTINCT over a lone max() keeps the min/max SEARCH access *and* adds the
        // DISTINCT sorter — the two are independent.
        (
            "SELECT DISTINCT max(n) FROM c",
            "CO-ROUTINE c | SETUP | SCAN CONSTANT ROW | RECURSIVE STEP | SCAN c | SEARCH c | \
             USE TEMP B-TREE FOR DISTINCT",
        ),
    ];
    for (tail, expected) in cases {
        let q = alloc_concat(BASE, tail);
        assert_eq!(g_eqp(D, &q), expected, "EQP mismatch for {q}");
        assert_eq!(g_eqp(D, &q), sqlite_eqp(D, &q), "EQP vs sqlite for {q}");
        assert_eq!(g_rows(D, &q), sqlite_rows(D, &q), "rows diverged for {q}");
    }
}

fn alloc_concat(a: &str, b: &str) -> String {
    let mut s = String::from(a);
    s.push_str(b);
    s
}
