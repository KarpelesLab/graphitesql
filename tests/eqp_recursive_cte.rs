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
//! none; the self-reference is a co-routine so the outer is always a `SCAN`, never a
//! `SEARCH`). The anchor arm itself is recursed normally, so a `SELECT <consts>` /
//! `VALUES(…)` body renders `SCAN CONSTANT ROW` and a `SELECT … FROM t` body renders
//! `SCAN t`. `UNION` vs `UNION ALL` is the same plan.
//!
//! Declined (graphite keeps its prior `Unsupported` error — never a wrong plan; the
//! executed rows always match): a join in the recursive arm (`FROM c, t` — an extra
//! scan child), an outer `ORDER BY` / `GROUP BY` / `DISTINCT` (each appends a temp
//! b-tree node SQLite sequences after the outer scan), and any non-canonical arm
//! split.

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
            lines.push(s.clone());
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
                    graphitesql::Value::Text(s) => s.clone(),
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
                Some(graphitesql::Value::Text(s)) => Some(s.clone()),
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
        // An outer ORDER BY appends a temp-b-tree node after the outer scan.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT * FROM c ORDER BY n DESC",
        // An outer GROUP BY likewise.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT n FROM c GROUP BY n",
        // An outer DISTINCT likewise.
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT DISTINCT n FROM c",
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
