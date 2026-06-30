//! Track B (EQP): the result-column analogue of the WHERE-clause scalar-subquery
//! slice (`tests/eqp_where_scalar_subquery.rs`). A non-correlated scalar
//! `(SELECT …)` in the SELECT list is rendered by SQLite as a `SCALAR SUBQUERY N`
//! node — a sibling of the outer scan, numbered left-to-right in column order,
//! with the subquery body's own plan recursed as its child. graphite previously
//! emitted no node at all (a bare `SCAN t`), diverging on every such query.
//!
//! The numbering is the same shared sequential id as the WHERE form, so it is only
//! a clean `1..n` — all we can render — when the statement has no CTEs, the
//! subqueries live solely in the projection, and each is a non-correlated,
//! non-compound scalar `(SELECT …)` over base tables with no further nested
//! subquery. The one wrinkle vs the WHERE form is *sequencing*: a projection
//! subquery is evaluated after grouping, so SQLite places its node *after* a
//! `USE TEMP B-TREE FOR GROUP BY` sorter (but still *before* DISTINCT / ORDER BY).
//! This file covers the no-GROUP-BY shapes, where our after-scan insertion point
//! matches; the GROUP BY projection case (rendered via a second insertion point
//! after the grouping sorter) lives in
//! `tests/eqp_grouped_projection_scalar_subquery.rs`. `DISTINCT` is declined
//! here: graphite's separate `USE TEMP B-TREE FOR DISTINCT` node does not fire when
//! a projection column is a subquery, so emitting the scalar node there would leave
//! the plan still diverging (missing the DISTINCT sorter) rather than byte-exact —
//! we never emit a node into a plan that does not fully match.
//!
//! Covered here, byte-exact vs sqlite3 3.50.4 (plan and rows): a lone projection
//! subquery (aggregate `min`/`count`, a constant `(SELECT 5)`, a filtered body);
//! two subqueries numbered `1`/`2` in column order, with intervening plain columns;
//! and a projection subquery alongside a non-subquery WHERE / ORDER BY / LIMIT.
//!
//! Deliberately declined (verified — graphite emits no `SCALAR SUBQUERY` node, the
//! rows stay correct): a subquery in `HAVING` (no projection node), `DISTINCT`
//! (graphite's distinct node does not fire), a correlated body
//! and `EXISTS` (a `CORRELATED` node), a compound (`UNION`) body (numbered past 1),
//! and a subquery in *both* the projection and the WHERE clause (cross-position
//! numbering — `SCALAR SUBQUERY 2` then `1`).

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

/// A non-correlated scalar `(SELECT …)` in the projection renders a
/// `SCALAR SUBQUERY N` node — byte-exact plan and rows.
#[test]
fn projection_scalar_subquery_renders_node() {
    if !have_sqlite() {
        return;
    }
    assert_eq!(
        g_eqp(D, "SELECT (SELECT count(*) FROM u) FROM t"),
        "SCAN t | SCALAR SUBQUERY 1 | SCAN u"
    );
    // Two subqueries are numbered 1 and 2 in column order, even with a plain
    // column sitting between them.
    assert_eq!(
        g_eqp(
            D,
            "SELECT (SELECT min(x) FROM u), a, (SELECT count(*) FROM u) FROM t"
        ),
        "SCAN t | SCALAR SUBQUERY 1 | SEARCH u | SCALAR SUBQUERY 2 | SCAN u"
    );
    for q in [
        "SELECT (SELECT count(*) FROM u) FROM t",
        "SELECT a, (SELECT min(x) FROM u) FROM t",
        "SELECT (SELECT count(*) FROM u), (SELECT max(y) FROM u) FROM t",
        "SELECT (SELECT min(x) FROM u), a, (SELECT count(*) FROM u) FROM t",
        // A constant FROM-less body → `SCAN CONSTANT ROW`.
        "SELECT (SELECT 5) FROM t",
        // A filtered scan body.
        "SELECT (SELECT x FROM u WHERE y>30) FROM t",
        // Alongside a non-subquery WHERE / ORDER BY / LIMIT (the node sits before
        // any ORDER BY sorter).
        "SELECT (SELECT count(*) FROM u) FROM t WHERE a>3",
        "SELECT a, (SELECT count(*) FROM u) FROM t ORDER BY a DESC",
        "SELECT (SELECT count(*) FROM u) FROM t WHERE a>3 ORDER BY b",
        "SELECT (SELECT count(*) FROM u) FROM t LIMIT 2",
    ] {
        let plan = g_eqp(D, q);
        assert!(
            plan.contains("SCALAR SUBQUERY 1"),
            "expected a SCALAR SUBQUERY node for {q}, got {plan}"
        );
        check(D, q);
    }
}

/// Declined shapes emit no `SCALAR SUBQUERY` node (graphite keeps its prior plan)
/// and the rows stay correct.
#[test]
fn projection_scalar_subquery_declines_unrenderable() {
    if !have_sqlite() {
        return;
    }
    for q in [
        // A subquery in HAVING (not the projection) → no projection node here; the
        // grouped-projection collector only fires for a subquery *in the projection*
        // (see `tests/eqp_grouped_projection_scalar_subquery.rs` for the rendered
        // GROUP BY projection case).
        "SELECT a FROM t GROUP BY a HAVING count(*)>(SELECT count(*) FROM u)",
        // DISTINCT: graphite's distinct sorter node does not fire here.
        "SELECT DISTINCT (SELECT count(*) FROM u), a FROM t",
        // Correlated body / EXISTS → CORRELATED SCALAR SUBQUERY.
        "SELECT (SELECT count(*) FROM u WHERE x=a) FROM t",
        "SELECT EXISTS(SELECT 1 FROM u WHERE x=a) FROM t",
        // A compound (UNION) body bumps the counter past 1.
        "SELECT (SELECT x FROM u UNION SELECT y FROM u) FROM t",
        // Subqueries in *both* the projection and the WHERE clause → cross-position
        // numbering we do not model.
        "SELECT (SELECT count(*) FROM u) FROM t WHERE a>(SELECT min(x) FROM u)",
    ] {
        assert!(
            !g_eqp(D, q).contains("SCALAR SUBQUERY"),
            "expected no SCALAR SUBQUERY node for the declined shape {q}"
        );
        assert_eq!(g_rows(D, q), sqlite_rows(D, q), "rows diverged for {q}");
    }
}
