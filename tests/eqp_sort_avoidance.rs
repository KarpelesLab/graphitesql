//! B9h sort-avoidance: a single-table query whose `WHERE` is not served by any
//! index but whose `ORDER BY` is, walks the ORDER-BY index to avoid a temp-b-tree
//! sort — `SCAN t USING INDEX i_b`, matching sqlite, instead of
//! `SCAN t` + `USE TEMP B-TREE FOR ORDER BY`. When the `WHERE` *is* served by a
//! seek index, that seek is planned instead (the sort stays), also matching
//! sqlite. Verified differentially against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

const SETUP: &str = "CREATE TABLE t(a, b, c); CREATE INDEX i_b ON t(b); \
                     INSERT INTO t VALUES(1,3,'x'),(2,1,'y'),(3,2,'z'),(4,1,'w');";

fn graphite_eqp(sql: &str) -> Vec<String> {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    c.query(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .rows
        .iter()
        .map(|r| match r.last() {
            Some(graphitesql::Value::Text(t)) => t.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn sqlite_eqp(sql: &str) -> Option<Vec<String>> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{SETUP} EXPLAIN QUERY PLAN {sql};"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            // sqlite renders EQP as a tree; strip the leading `|--` / `` `-- `` /
            // indentation markers to leave just each node's detail, and drop the
            // `QUERY PLAN` header (graphite's `query()` returns the bare details).
            .map(|l| {
                l.trim_start_matches(['`', '|', '-', ' ']).to_string()
            })
            .filter(|s| !s.is_empty() && s != "QUERY PLAN")
            .collect(),
    )
}

#[test]
fn order_by_index_avoids_sort_matching_sqlite() {
    if sqlite_eqp("SELECT 1").is_none() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Each query is `WHERE <not-indexed predicate> ORDER BY <indexed>` (sort
    // avoided via the index), a seek predicate (seek used, sort kept), or a
    // not-order-indexable ORDER BY (sort kept) — the graphite EQP must equal
    // sqlite's in every case.
    for q in [
        "SELECT * FROM t WHERE a>0 ORDER BY b",
        "SELECT * FROM t WHERE c='x' ORDER BY b",
        "SELECT a FROM t WHERE a>0 ORDER BY b",
        "SELECT * FROM t WHERE b=2 ORDER BY b",
        "SELECT * FROM t ORDER BY b",
        "SELECT * FROM t WHERE a>0 ORDER BY c",
        "SELECT * FROM t WHERE a>0 ORDER BY b DESC",
    ] {
        let want = sqlite_eqp(q).unwrap();
        let got = graphite_eqp(q);
        assert_eq!(got, want, "EQP diverged from sqlite on `{q}`");
    }
}

#[test]
fn sort_avoided_results_are_correctly_ordered() {
    if sqlite_eqp("SELECT 1").is_none() {
        return;
    }
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    // The index-walk path (WHERE filtered downstream) still yields correctly
    // ordered rows.
    let rows = c
        .query("SELECT a, b FROM t WHERE a > 0 ORDER BY b")
        .unwrap()
        .rows;
    let bs: Vec<i64> = rows
        .iter()
        .map(|r| match &r[1] {
            graphitesql::Value::Integer(n) => *n,
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(bs, vec![1, 1, 2, 3], "rows must be in ascending b order");
}
