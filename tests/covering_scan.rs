//! A query with no WHERE whose every referenced column is held by exactly one
//! full secondary index is answered by a covering scan of that index (B0b-ii and
//! plain covered projections) — EXPLAIN QUERY PLAN reads `SCAN t USING COVERING
//! INDEX i`, matching sqlite3 3.50.4 — while results stay correct. Ambiguous or
//! uncovered cases keep the plain `SCAN t`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn plan(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match r.last() {
            Some(Value::Text(s)) => s.clone(),
            other => panic!("detail not text: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(a, b, c)").unwrap();
    c.execute("CREATE INDEX iu ON u(a, b)").unwrap();
    c.execute("INSERT INTO u VALUES (1,3,'x'),(1,2,'y'),(2,1,'z')")
        .unwrap();
    c
}

#[test]
fn plain_covered_projection_uses_covering_index() {
    let c = setup();
    assert_eq!(
        plan(&c, "EXPLAIN QUERY PLAN SELECT a, b FROM u"),
        "SCAN u USING COVERING INDEX iu"
    );
}

#[test]
fn distinct_over_covered_column() {
    let c = setup();
    assert_eq!(
        plan(&c, "EXPLAIN QUERY PLAN SELECT DISTINCT a FROM u"),
        "SCAN u USING COVERING INDEX iu"
    );
    let mut got: Vec<i64> = c
        .query("SELECT DISTINCT a FROM u")
        .unwrap()
        .rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            _ => panic!(),
        })
        .collect();
    got.sort();
    assert_eq!(got, [1, 2]);
}

#[test]
fn group_by_aggregate_over_covered_columns() {
    let c = setup();
    assert_eq!(
        plan(
            &c,
            "EXPLAIN QUERY PLAN SELECT a, count(*) FROM u GROUP BY a"
        ),
        "SCAN u USING COVERING INDEX iu"
    );
    assert_eq!(
        c.query("SELECT a, count(*) FROM u GROUP BY a ORDER BY a")
            .unwrap()
            .rows,
        [
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(2), Value::Integer(1)],
        ]
    );
}

#[test]
fn order_by_needing_a_sort_still_covers() {
    let c = setup();
    // ORDER BY a non-leading covered column can't be supplied by the index walk,
    // so the index covers the read and a temp b-tree sorts.
    let p = plan(&c, "EXPLAIN QUERY PLAN SELECT a FROM u ORDER BY b");
    assert!(p.contains("USING COVERING INDEX iu"), "got {p}");
    assert!(p.contains("USE TEMP B-TREE"), "got {p}");
}

#[test]
fn uncovered_column_keeps_plain_scan() {
    let c = setup();
    // `c` is not in the index.
    assert_eq!(plan(&c, "EXPLAIN QUERY PLAN SELECT a, c FROM u"), "SCAN u");
    assert_eq!(plan(&c, "EXPLAIN QUERY PLAN SELECT * FROM u"), "SCAN u");
}

#[test]
fn order_by_rowid_keeps_table_scan() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    c.execute("CREATE INDEX ib ON t(b)").unwrap();
    c.execute("INSERT INTO t VALUES (1,30),(2,10),(3,20)")
        .unwrap();
    // ORDER BY the rowid is satisfied by the table scan's natural order — switching
    // to the covering index would reorder, so the plain scan is kept.
    assert_eq!(
        plan(&c, "EXPLAIN QUERY PLAN SELECT a FROM t ORDER BY a"),
        "SCAN t"
    );
    assert_eq!(
        c.query("SELECT a FROM t ORDER BY a").unwrap().rows,
        [
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );
}

#[test]
fn ambiguous_covering_indexes_keep_plain_scan() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b, c)")
        .unwrap();
    c.execute("CREATE INDEX ib ON t(b)").unwrap();
    c.execute("CREATE INDEX ic ON t(c)").unwrap();
    c.execute("INSERT INTO t VALUES (1,10,100),(2,20,200)")
        .unwrap();
    // count(*) is covered by both ib and ic; rather than guess sqlite's pick,
    // graphite keeps the plain scan.
    assert_eq!(
        plan(&c, "EXPLAIN QUERY PLAN SELECT count(*) FROM t"),
        "SCAN t"
    );
}
