//! A multi-term `ORDER BY` whose columns are the leading columns of a secondary
//! index is satisfied by walking that index — no temp b-tree — when all terms
//! share one direction (roadmap B0b-i). EXPLAIN QUERY PLAN reads `SCAN t USING
//! [COVERING] INDEX i` with no `USE TEMP B-TREE`, matching sqlite3 3.50.4, and the
//! rows are correctly ordered. A mixed-direction ORDER BY still sorts.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn plan(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match r.last() {
            Some(Value::Text(s)) => s.clone(),
            other => panic!("detail not text: {other:?}"),
        })
        .collect()
}

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(a, b, c)").unwrap();
    c.execute("CREATE INDEX iu ON u(a, b)").unwrap();
    c.execute("INSERT INTO u VALUES (1,3,'x'),(1,2,'y'),(2,1,'z'),(1,1,'w'),(2,5,'m')")
        .unwrap();
    c
}

#[test]
fn ascending_multi_term_uses_index_no_sort() {
    let c = setup();
    assert_eq!(
        plan(&c, "EXPLAIN QUERY PLAN SELECT c FROM u ORDER BY a, b"),
        ["SCAN u USING INDEX iu"]
    );
    assert_eq!(
        rows(&c, "SELECT c FROM u ORDER BY a, b"),
        [
            vec![Value::Text("w".into())],
            vec![Value::Text("y".into())],
            vec![Value::Text("x".into())],
            vec![Value::Text("z".into())],
            vec![Value::Text("m".into())],
        ]
    );
}

#[test]
fn descending_multi_term_walks_index_reversed() {
    let c = setup();
    assert_eq!(
        plan(
            &c,
            "EXPLAIN QUERY PLAN SELECT c FROM u ORDER BY a DESC, b DESC"
        ),
        ["SCAN u USING INDEX iu"]
    );
    assert_eq!(
        rows(&c, "SELECT c FROM u ORDER BY a DESC, b DESC"),
        [
            vec![Value::Text("m".into())],
            vec![Value::Text("z".into())],
            vec![Value::Text("x".into())],
            vec![Value::Text("y".into())],
            vec![Value::Text("w".into())],
        ]
    );
}

#[test]
fn covering_multi_term_reads_index_only() {
    let c = setup();
    assert_eq!(
        plan(&c, "EXPLAIN QUERY PLAN SELECT a, b FROM u ORDER BY a, b"),
        ["SCAN u USING COVERING INDEX iu"]
    );
}

#[test]
fn mixed_direction_still_sorts() {
    let c = setup();
    // The index can't supply `a ASC, b DESC` from a single forward/backward walk,
    // so graphite sorts (results stay correct).
    let p = plan(&c, "EXPLAIN QUERY PLAN SELECT c FROM u ORDER BY a, b DESC");
    assert!(
        p.iter().any(|s| s.contains("USE TEMP B-TREE")),
        "expected a sort, got {p:?}"
    );
    assert_eq!(
        rows(&c, "SELECT c FROM u ORDER BY a, b DESC"),
        [
            vec![Value::Text("x".into())],
            vec![Value::Text("y".into())],
            vec![Value::Text("w".into())],
            vec![Value::Text("m".into())],
            vec![Value::Text("z".into())],
        ]
    );
}

#[test]
fn explicit_nulls_ordering_opts_out() {
    let c = setup();
    // An explicit NULLS placement may disagree with the index's natural order, so
    // it falls back to a sort rather than risk a wrong order.
    let p = plan(
        &c,
        "EXPLAIN QUERY PLAN SELECT c FROM u ORDER BY a, b NULLS LAST",
    );
    assert!(
        p.iter().any(|s| s.contains("USE TEMP B-TREE")),
        "expected a sort, got {p:?}"
    );
}
