//! Track B (VDBE row order): with no `WHERE` and no `ORDER BY`, SQLite may answer
//! a query by reading a *covering* secondary index — `SELECT a FROM t` over an
//! index on `a` reads `SCAN t USING COVERING INDEX ia`, emitting rows in `a`
//! (index-key) order rather than rowid order. The VDBE executes a single-table
//! `SELECT` as a rowid-order table scan, so its output order would diverge
//! whenever the covering index is stored out of rowid order and there is no
//! `ORDER BY` to re-sort.
//!
//! `run_select_vdbe` now defers exactly those queries to the tree-walker (via
//! `vdbe_covering_scan_reorders`, which mirrors `covering_scan`'s applicability),
//! whose covering scan walks the index in key order — matching SQLite. A query
//! whose covering index is not picked (ambiguous, or not covering, e.g.
//! `SELECT *`), or that has an `ORDER BY` re-sorting the rows, stays on the VDBE
//! and still matches. Every case is checked against sqlite3 3.50.4, both the plan
//! and the result rows (row order is the whole point here).

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

#[test]
fn single_column_covering_scan_in_index_order() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // `a` is indexed but stored out of rowid order, so a covering index walk and a
    // rowid scan give different orders. With no WHERE/ORDER BY, SQLite covering-scans
    // the index — the VDBE must defer to reproduce that order.
    let d = "CREATE TABLE t(a, b, c); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(5,1,1),(2,2,2),(8,3,3),(2,4,4),(7,5,5);";
    for q in [
        "SELECT a FROM t",
        "SELECT DISTINCT a FROM t",
        // A LIMIT without ORDER BY exposes which rows come *first* — they must be
        // the first in index order, not rowid order.
        "SELECT a FROM t LIMIT 3",
        // `SELECT *` is not covered by `ia`, so it stays a rowid `SCAN` on both
        // sides (regression guard — must not start covering-scanning).
        "SELECT * FROM t",
        "SELECT b FROM t",
        // An explicit ORDER BY fixes the order regardless of the access path.
        "SELECT a FROM t ORDER BY a",
        "SELECT a FROM t ORDER BY b",
    ] {
        check(d, q);
    }
}

#[test]
fn composite_covering_scan_in_index_order() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // A composite index `(a, b)` covers `SELECT a, b` and is walked in (a, b)
    // order — distinct from rowid order.
    let d = "CREATE TABLE t(a, b, c); CREATE INDEX iab ON t(a, b); \
             INSERT INTO t VALUES(5,1,1),(2,2,2),(8,3,3),(2,4,4),(7,5,5);";
    for q in [
        "SELECT a, b FROM t",
        "SELECT b, a FROM t",
        "SELECT DISTINCT a, b FROM t",
        // `c` is not covered — plain rowid SCAN on both sides.
        "SELECT a, c FROM t",
    ] {
        check(d, q);
    }
}

#[test]
fn covering_scan_with_group_or_agg_stays_correct() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // `count(*)` and `GROUP BY a` over the covering index keep matching SQLite —
    // the count is order-independent and the grouped output is in key order.
    let d = "CREATE TABLE t(a, b, c); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(5,1,1),(2,2,2),(8,3,3),(2,4,4),(7,5,5);";
    for q in [
        "SELECT count(*) FROM t",
        "SELECT a FROM t GROUP BY a",
        "SELECT a, count(*) FROM t GROUP BY a",
    ] {
        check(d, q);
    }
}
