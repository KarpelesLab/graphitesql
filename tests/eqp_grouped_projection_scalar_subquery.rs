//! Track B (EQP): the GROUP BY analogue of the projection scalar-subquery slice
//! (`tests/eqp_projection_scalar_subquery.rs`). A non-correlated scalar `(SELECT …)`
//! in the projection of a *grouped* query is rendered by SQLite as a
//! `SCALAR SUBQUERY N` node — but, unlike the un-grouped form, it is sequenced
//! *after* the grouping sorter (`USE TEMP B-TREE FOR GROUP BY`) and still *before*
//! an ORDER BY sorter. graphite emits the grouping and ordering nodes correctly but
//! previously emitted no scalar node, diverging on every such query.
//!
//! A second insertion point — right after the GROUP BY temp b-tree, before the
//! ORDER BY temp b-tree — matches SQLite. As with the other positions, the numbering
//! is a shared sequential id, so it is only a clean `1..n` when the subqueries live
//! solely in the projection (one in WHERE / HAVING / ORDER BY / LIMIT would shift or
//! reorder the ids) and each is a non-correlated, non-compound scalar `(SELECT …)`
//! over base tables with no nested subquery.
//!
//! Declined (a different insertion point, reorder, or counter shift): `DISTINCT` (a
//! separate `USE TEMP B-TREE FOR DISTINCT` sorter whose interplay we do not model), a
//! subquery in `HAVING` (SQLite numbers the HAVING one first and renders the nodes in
//! a reversed order), a correlated body (`CORRELATED SCALAR SUBQUERY`), a compound
//! (`UNION`) body, and a join inside the body.
//!
//! Covered here, byte-exact vs sqlite3 3.50.4 (plan and rows): a lone projection
//! subquery in a `GROUP BY` (either column position); two subqueries numbered `1`/`2`
//! in column order; a constant `(SELECT 5)` and a filtered-scan body; alongside a
//! non-subquery `HAVING`; and folded with an `ORDER BY` that does or does not need its
//! own sorter.

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

/// A projection scalar subquery in a `GROUP BY` query renders a `SCALAR SUBQUERY N`
/// node *after* the grouping sorter — byte-exact plan and rows.
#[test]
fn grouped_projection_scalar_subquery_renders_after_group_sorter() {
    if !have_sqlite() {
        return;
    }
    assert_eq!(
        g_eqp(D, "SELECT (SELECT min(x) FROM u), a FROM t GROUP BY a"),
        "SCAN t | USE TEMP B-TREE FOR GROUP BY | SCALAR SUBQUERY 1 | SEARCH u"
    );
    // Two projection subqueries are numbered 1 and 2 in column order, both after
    // the grouping sorter.
    assert_eq!(
        g_eqp(
            D,
            "SELECT (SELECT min(x) FROM u), (SELECT max(y) FROM u), a FROM t GROUP BY a"
        ),
        "SCAN t | USE TEMP B-TREE FOR GROUP BY | SCALAR SUBQUERY 1 | SEARCH u | \
         SCALAR SUBQUERY 2 | SEARCH u"
    );
    // Folded with an ORDER BY that needs its own sorter: the scalar node sits
    // between the GROUP BY and ORDER BY b-trees.
    assert_eq!(
        g_eqp(
            D,
            "SELECT (SELECT min(x) FROM u), a FROM t GROUP BY a ORDER BY b"
        ),
        "SCAN t | USE TEMP B-TREE FOR GROUP BY | SCALAR SUBQUERY 1 | SEARCH u | \
         USE TEMP B-TREE FOR ORDER BY"
    );
    for q in [
        // Either column position.
        "SELECT (SELECT min(x) FROM u), a FROM t GROUP BY a",
        "SELECT a, (SELECT min(x) FROM u) FROM t GROUP BY a",
        // A constant FROM-less body → `SCAN CONSTANT ROW`.
        "SELECT (SELECT 5), a FROM t GROUP BY a",
        // A filtered-scan body.
        "SELECT (SELECT x FROM u WHERE y>30), a FROM t GROUP BY a",
        // count(*) aggregate body.
        "SELECT (SELECT count(*) FROM u), a FROM t GROUP BY a",
        // Alongside a non-subquery HAVING.
        "SELECT (SELECT min(x) FROM u), a FROM t GROUP BY a HAVING a>1",
        // Folded with an ORDER BY satisfied by the GROUP BY (no separate sorter).
        "SELECT (SELECT min(x) FROM u), a FROM t GROUP BY a ORDER BY a",
        "SELECT (SELECT min(x) FROM u), a FROM t GROUP BY a ORDER BY a DESC",
        // HAVING (non-subquery) and an ORDER BY that needs a sorter.
        "SELECT (SELECT min(x) FROM u), a FROM t GROUP BY a HAVING a>0 ORDER BY b",
        // A non-aggregate result column riding along.
        "SELECT a, count(*), (SELECT max(y) FROM u) FROM t GROUP BY a",
    ] {
        let plan = g_eqp(D, q);
        assert!(
            plan.contains("SCALAR SUBQUERY 1"),
            "expected a SCALAR SUBQUERY node for {q}, got {plan}"
        );
        // The node must follow the grouping sorter, never precede it.
        assert!(
            plan.find("USE TEMP B-TREE FOR GROUP BY").unwrap()
                < plan.find("SCALAR SUBQUERY 1").unwrap(),
            "SCALAR SUBQUERY must follow the GROUP BY sorter for {q}, got {plan}"
        );
        check(D, q);
    }
}

/// Declined shapes emit no `SCALAR SUBQUERY` node (graphite keeps its prior plan)
/// and the rows stay correct.
#[test]
fn grouped_projection_scalar_subquery_declines_unrenderable() {
    if !have_sqlite() {
        return;
    }
    for q in [
        // DISTINCT: a separate distinct sorter whose interplay we decline.
        "SELECT DISTINCT (SELECT min(x) FROM u), a FROM t GROUP BY a",
        // A subquery in HAVING reorders / renumbers the nodes.
        "SELECT (SELECT min(x) FROM u), a FROM t GROUP BY a HAVING a>(SELECT min(x) FROM u)",
        // Correlated body → CORRELATED SCALAR SUBQUERY.
        "SELECT (SELECT count(*) FROM u WHERE x=a), a FROM t GROUP BY a",
        // A compound (UNION) body bumps the counter past 1.
        "SELECT (SELECT x FROM u UNION SELECT y FROM u), a FROM t GROUP BY a",
        // A join inside the body — a multi-scan child we do not model.
        "SELECT (SELECT count(*) FROM u, t t2), a FROM t GROUP BY a",
        // A subquery in ORDER BY → cross-position numbering.
        "SELECT (SELECT min(x) FROM u), a FROM t GROUP BY a ORDER BY (SELECT max(y) FROM u)",
    ] {
        assert!(
            !g_eqp(D, q).contains("SCALAR SUBQUERY"),
            "expected no SCALAR SUBQUERY node for the declined shape {q}, got {}",
            g_eqp(D, q)
        );
        assert_eq!(g_rows(D, q), sqlite_rows(D, q), "rows diverged for {q}");
    }
}
