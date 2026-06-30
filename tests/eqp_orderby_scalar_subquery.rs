//! Track B (EQP): the `ORDER BY` analogue of the WHERE / projection scalar-subquery
//! slices (`tests/eqp_where_scalar_subquery.rs`,
//! `tests/eqp_projection_scalar_subquery.rs`). A non-correlated scalar `(SELECT …)`
//! in an `ORDER BY` term is rendered by SQLite as a `SCALAR SUBQUERY N` node — a
//! sibling of the outer scan, numbered left-to-right in term order, with the
//! subquery body's own plan recursed as its child. graphite previously emitted no
//! node at all, diverging on every such query.
//!
//! An `ORDER BY` subquery is sequenced exactly like a WHERE one — after the scan and
//! before the `USE TEMP B-TREE FOR ORDER BY` sorter — so our single insertion point
//! right after the scan matches SQLite. As with the other forms, the numbering is a
//! shared sequential id, so it is only a clean `1..n` when the statement has no CTEs,
//! the subqueries live solely in `ORDER BY`, and each is a non-correlated,
//! non-compound scalar `(SELECT …)` over base tables with no nested subquery.
//!
//! Declined (a different insertion point or counter shift): `GROUP BY` / `HAVING`
//! (node sequenced after the grouping sorter), `DISTINCT` (a separate
//! `USE TEMP B-TREE FOR DISTINCT` sorter whose interplay with ORDER BY we do not
//! model), a correlated body / `EXISTS`, a compound (`UNION`) body, a join inside
//! the body, and a subquery in another clause (cross-position numbering).
//!
//! Covered here, byte-exact vs sqlite3 3.50.4 (plan and rows): a lone ORDER BY
//! subquery (aggregate `min`/`count`, a constant `(SELECT 5)`, a filtered body);
//! two subqueries numbered `1`/`2` in term order; an ORDER BY subquery mixed with a
//! plain ordering column and a non-subquery WHERE / LIMIT; and `DESC` directions.

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

/// A non-correlated scalar `(SELECT …)` in an `ORDER BY` term renders a
/// `SCALAR SUBQUERY N` node — byte-exact plan and rows.
#[test]
fn orderby_scalar_subquery_renders_node() {
    if !have_sqlite() {
        return;
    }
    assert_eq!(
        g_eqp(D, "SELECT a FROM t ORDER BY (SELECT min(x) FROM u)"),
        "SCAN t | SCALAR SUBQUERY 1 | SEARCH u | USE TEMP B-TREE FOR ORDER BY"
    );
    // Two ORDER BY subqueries are numbered 1 and 2 in term order.
    assert_eq!(
        g_eqp(
            D,
            "SELECT a FROM t ORDER BY (SELECT min(x) FROM u), (SELECT max(y) FROM u)"
        ),
        "SCAN t | SCALAR SUBQUERY 1 | SEARCH u | SCALAR SUBQUERY 2 | SEARCH u | \
         USE TEMP B-TREE FOR ORDER BY"
    );
    for q in [
        "SELECT a FROM t ORDER BY (SELECT min(x) FROM u)",
        "SELECT a FROM t ORDER BY (SELECT count(*) FROM u)",
        // A constant FROM-less body → `SCAN CONSTANT ROW`.
        "SELECT a FROM t ORDER BY (SELECT 5)",
        // A filtered scan body.
        "SELECT a FROM t ORDER BY (SELECT x FROM u WHERE y>30)",
        // Mixed with a plain ordering column, either side.
        "SELECT a FROM t ORDER BY a, (SELECT min(x) FROM u)",
        "SELECT a FROM t ORDER BY (SELECT min(x) FROM u), a",
        // Alongside a non-subquery WHERE / LIMIT.
        "SELECT a FROM t WHERE b>0 ORDER BY (SELECT min(x) FROM u)",
        "SELECT a FROM t ORDER BY (SELECT min(x) FROM u) LIMIT 2",
        // Explicit direction on the subquery term.
        "SELECT a FROM t ORDER BY (SELECT min(x) FROM u) DESC, (SELECT max(y) FROM u)",
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
fn orderby_scalar_subquery_declines_unrenderable() {
    if !have_sqlite() {
        return;
    }
    for q in [
        // GROUP BY / HAVING: SQLite sequences the node *after* the grouping sorter.
        "SELECT a FROM t GROUP BY a ORDER BY (SELECT min(x) FROM u)",
        "SELECT a FROM t GROUP BY a HAVING a>0 ORDER BY (SELECT min(x) FROM u)",
        // DISTINCT: a separate distinct sorter whose ORDER BY interplay we decline.
        "SELECT DISTINCT a FROM t ORDER BY (SELECT min(x) FROM u)",
        // Correlated body / EXISTS → CORRELATED SCALAR SUBQUERY.
        "SELECT a FROM t ORDER BY (SELECT count(*) FROM u WHERE x=a)",
        "SELECT a FROM t ORDER BY EXISTS(SELECT 1 FROM u WHERE x=a)",
        // A compound (UNION) body bumps the counter past 1.
        "SELECT a FROM t ORDER BY (SELECT x FROM u UNION SELECT y FROM u)",
        // A join inside the body — a multi-scan child we do not model.
        "SELECT a FROM t ORDER BY (SELECT count(*) FROM u, t t2)",
        // A subquery in another clause → cross-position numbering.
        "SELECT a FROM t WHERE a>(SELECT min(x) FROM u) ORDER BY (SELECT max(y) FROM u)",
    ] {
        assert!(
            !g_eqp(D, q).contains("SCALAR SUBQUERY"),
            "expected no SCALAR SUBQUERY node for the declined shape {q}"
        );
        assert_eq!(g_rows(D, q), sqlite_rows(D, q), "rows diverged for {q}");
    }
}
