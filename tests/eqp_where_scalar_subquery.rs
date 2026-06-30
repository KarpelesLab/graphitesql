//! Track B (EQP): a non-correlated scalar `(SELECT …)` in the `WHERE` clause is
//! computed once and rendered by SQLite as a `SCALAR SUBQUERY N` node — a sibling
//! of the outer scan, numbered left-to-right, with the subquery body's own plan as
//! its child, placed after the scan and before any GROUP BY / ORDER BY sorter.
//! graphite previously emitted no node at all (a bare `SCAN t`), diverging from
//! SQLite on every such query.
//!
//! SQLite assigns every subquery in a statement a sequential id shared with CTE
//! materialisations and compound arms, so the numbering is only a clean `1..n` —
//! which is all we can render — when the query has no CTEs, the subqueries live
//! solely in the `WHERE` clause, and each is a non-correlated, non-compound scalar
//! `(SELECT …)` over base tables with no further nested subquery. We emit exactly
//! that subset and decline everything else (leaving the pre-existing `SCAN t`).
//!
//! Covered here, byte-exact vs sqlite3 3.50.4 (plan and rows): a scalar subquery
//! compared by `=` / `>` against a column; an aggregate body (`min`/`count`, whose
//! min/max body renders `SEARCH`), a constant body (`(SELECT 5)` → `SCAN CONSTANT
//! ROW`), and a filtered body; two subqueries numbered `1` and `2`; and a subquery
//! alongside a GROUP BY / ORDER BY node (correct sibling order).
//!
//! Deliberately still declined (verified unchanged — still the bare `SCAN t`):
//!  * a `CORRELATED` body (reads an outer column) and `EXISTS` (a different node).
//!  * `IN (SELECT …)` (a `LIST SUBQUERY` + `CREATE BLOOM FILTER`).
//!  * a body that bumps the id counter past `1..n`: a CTE reference or a compound
//!    (`UNION`) body (SQLite numbers these `SCALAR SUBQUERY 2`).
//!  * a join inside the body (SQLite renders a multi-scan child we do not model).
//!
//! (A scalar subquery in the *projection* is rendered by the companion slice —
//! `tests/eqp_projection_scalar_subquery.rs`.)

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn norm(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).trim_end())
        .collect::<Vec<_>>()
        .join(" | ")
}

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
    norm(&String::from_utf8_lossy(&o.stdout))
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
                    graphitesql::Value::Real(f) => {
                        let s = format!("{f}");
                        if s.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                            format!("{s}.0")
                        } else {
                            s
                        }
                    }
                    graphitesql::Value::Text(s) => s.clone(),
                    graphitesql::Value::Blob(_) => "<blob>".to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
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

const D: &str = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9); \
    CREATE TABLE u(x,y); INSERT INTO u VALUES(1,20),(4,50);";

/// A non-correlated scalar `(SELECT …)` in the `WHERE` clause renders as a
/// `SCALAR SUBQUERY N` node — byte-exact plan and rows.
#[test]
fn where_scalar_subquery_renders_node() {
    if !have_sqlite() {
        return;
    }
    // The shape: scan, then the SCALAR SUBQUERY child carrying the body's plan.
    assert_eq!(
        g_eqp(D, "SELECT * FROM t WHERE a=(SELECT min(x) FROM u)"),
        "SCAN t | SCALAR SUBQUERY 1 | SEARCH u"
    );
    for q in [
        // Aggregate bodies: min/max render `SEARCH`, count renders `SCAN`.
        "SELECT * FROM t WHERE a=(SELECT min(x) FROM u)",
        "SELECT * FROM t WHERE a=(SELECT count(*) FROM u)",
        "SELECT * FROM t WHERE b>(SELECT count(*) FROM u)",
        // A constant FROM-less body → `SCAN CONSTANT ROW`.
        "SELECT * FROM t WHERE a=(SELECT 5)",
        // A filtered scan body.
        "SELECT c FROM t WHERE a=(SELECT x FROM u WHERE y>30)",
        // Two subqueries — numbered 1 and 2, left-to-right.
        "SELECT * FROM t WHERE a=(SELECT min(x) FROM u) AND b<(SELECT max(y) FROM u)",
        "SELECT * FROM t WHERE a=(SELECT min(x) FROM u) AND b=(SELECT 5)",
        // The subquery node precedes a GROUP BY / ORDER BY sorter node.
        "SELECT b FROM t WHERE a=(SELECT min(x) FROM u) GROUP BY b",
        "SELECT * FROM t WHERE a=(SELECT min(x) FROM u) ORDER BY b DESC",
        // Mixed with an ordinary IN-list predicate (no subquery).
        "SELECT c FROM t WHERE a IN (1,4) AND b=(SELECT max(y) FROM u)",
    ] {
        let plan = g_eqp(D, q);
        assert!(
            plan.contains("SCALAR SUBQUERY 1"),
            "expected a SCALAR SUBQUERY node for {q}, got {plan}"
        );
        check(D, q);
    }
}

/// Shapes we deliberately decline keep the pre-existing bare `SCAN t` (no node) —
/// either a different node kind we don't model, or a body that shifts SQLite's
/// subquery numbering past the clean `1..n` we render. The rows stay correct.
#[test]
fn where_scalar_subquery_declines_unrenderable() {
    if !have_sqlite() {
        return;
    }
    for q in [
        // Correlated body (reads outer `a`) → CORRELATED SCALAR SUBQUERY.
        "SELECT * FROM t WHERE a=(SELECT x FROM u WHERE x=a)",
        // EXISTS → CORRELATED SCALAR SUBQUERY (a different node).
        "SELECT * FROM t WHERE EXISTS(SELECT 1 FROM u WHERE x=a)",
        // IN (SELECT) → LIST SUBQUERY + CREATE BLOOM FILTER.
        "SELECT * FROM t WHERE b IN (SELECT x FROM u)",
        // A CTE reference bumps the id counter → SQLite numbers it 2.
        "WITH cte AS (SELECT x FROM u) SELECT * FROM t WHERE a=(SELECT min(x) FROM cte)",
        // A compound (UNION) body bumps the counter too.
        "SELECT * FROM t WHERE a=(SELECT x FROM u UNION SELECT y FROM u)",
        // A join inside the body — SQLite renders the node with a multi-scan child
        // we do not model, so the whole set is declined.
        "SELECT * FROM t WHERE a=(SELECT count(*) FROM u, t t2)",
    ] {
        assert_eq!(
            g_eqp(D, q),
            "SCAN t",
            "expected the unchanged bare SCAN for the declined shape {q}"
        );
        assert_eq!(g_rows(D, q), sqlite_rows(D, q), "rows diverged for {q}");
    }
}
