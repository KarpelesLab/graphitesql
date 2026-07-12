//! Planner seeks over *partial* and *expression* indexes (roadmap A3).
//!
//! A partial index (`CREATE INDEX … WHERE pred`) is only a valid seek index when
//! the query's `WHERE` guarantees `pred` as a top-level `AND` conjunct; an
//! expression index (`CREATE INDEX … (expr)`) is usable when a conjunct equates
//! the indexed expression to a value. These tests assert the plan reflects the
//! seek (`SEARCH … USING INDEX <name>`) when usable, a `SCAN` when not, and that
//! the returned rows are correct in every case (the seek returns a superset that
//! the full `WHERE` re-filters).
#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup(sql: &str) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in sql.split(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

/// The single-cell text of the EQP detail column (last column of the last row).
fn plan(c: &mut Connection, sql: &str) -> String {
    let r = c.query(sql).unwrap();
    let row = r.rows.last().expect("a query-plan row");
    match row.last().unwrap() {
        Value::Text(s) => String::from(s.as_str()),
        other => panic!("plan detail not text: {other:?}"),
    }
}

/// All single-column integer results of `sql`, sorted for order-independence.
fn ints(c: &mut Connection, sql: &str) -> Vec<i64> {
    let r = c.query(sql).unwrap();
    let mut out: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Integer(i) => *i,
            other => panic!("not an integer: {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

#[test]
fn partial_index_used_when_predicate_is_a_conjunct() {
    let mut c = setup(
        "CREATE TABLE t(a, b, active);
         CREATE INDEX ip ON t(a) WHERE active=1;
         INSERT INTO t VALUES(5,1,1),(5,2,0),(6,3,1);",
    );

    // Predicate guaranteed by a top-level conjunct → the partial index seeks.
    let p = plan(
        &mut c,
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a=5 AND active=1",
    );
    assert!(p.contains("USING INDEX ip"), "expected seek, got: {p}");

    // Conjunct order is irrelevant (structural membership, not position).
    let p = plan(
        &mut c,
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE active=1 AND a=5",
    );
    assert!(p.contains("USING INDEX ip"), "expected seek, got: {p}");

    // Correct rows: only (5,1,1) has a=5 AND active=1.
    assert_eq!(
        ints(&mut c, "SELECT b FROM t WHERE a=5 AND active=1"),
        vec![1]
    );
}

#[test]
fn partial_index_not_used_when_predicate_absent() {
    let mut c = setup(
        "CREATE TABLE t(a, b, active);
         CREATE INDEX ip ON t(a) WHERE active=1;
         INSERT INTO t VALUES(5,1,1),(5,2,0),(6,3,1);",
    );

    // No `active=1` conjunct → the partial index is NOT a valid seek (it omits
    // the inactive rows), so the planner must scan.
    let p = plan(&mut c, "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a=5");
    assert_eq!(p, "SCAN t", "partial index must not be used: {p}");

    // Results are still correct: both a=5 rows (active and inactive) come back.
    assert_eq!(ints(&mut c, "SELECT b FROM t WHERE a=5"), vec![1, 2]);
}

#[test]
fn expression_index_used_and_correct() {
    let mut c = setup(
        "CREATE TABLE t(name);
         CREATE INDEX ie ON t(lower(name));
         INSERT INTO t VALUES('Ann'),('BOB'),('bob');",
    );

    // `lower(name) = <value>` matches the index key expression → seek.
    let p = plan(
        &mut c,
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE lower(name)='bob'",
    );
    assert!(p.contains("USING INDEX ie"), "expected seek, got: {p}");

    // Either operand order works.
    let p = plan(
        &mut c,
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE 'bob'=lower(name)",
    );
    assert!(p.contains("USING INDEX ie"), "expected seek, got: {p}");

    // Both 'BOB' and 'bob' satisfy lower(name)='bob'.
    let r = c
        .query("SELECT name FROM t WHERE lower(name)='bob'")
        .unwrap();
    let mut names: Vec<String> = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Text(s) => String::from(s.as_str()),
            other => panic!("not text: {other:?}"),
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["BOB".to_string(), "bob".to_string()]);
}

#[test]
fn expression_index_not_used_for_plain_column_predicate() {
    let mut c = setup(
        "CREATE TABLE t(name);
         CREATE INDEX ie ON t(lower(name));
         INSERT INTO t VALUES('Ann'),('BOB');",
    );

    // A predicate on the raw column (not the indexed expression) can't use ie.
    let p = plan(
        &mut c,
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE name='Ann'",
    );
    assert_eq!(p, "SCAN t", "expression index must not be used: {p}");

    let r = c.query("SELECT name FROM t WHERE name='Ann'").unwrap();
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn plain_index_unaffected() {
    let mut c = setup(
        "CREATE TABLE t(a, b);
         CREATE INDEX ia ON t(a);
         INSERT INTO t VALUES(5,1),(5,2),(6,3);",
    );

    // Ordinary column index still seeks exactly as before.
    let p = plan(&mut c, "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a=5");
    assert!(p.contains("USING INDEX ia"), "expected seek, got: {p}");
    assert_eq!(ints(&mut c, "SELECT b FROM t WHERE a=5"), vec![1, 2]);
}

#[test]
fn partial_expression_index_combined() {
    // A partial *expression* index: both proofs must hold (predicate conjunct AND
    // the expression-equality conjunct).
    let mut c = setup(
        "CREATE TABLE t(name, live);
         CREATE INDEX ipe ON t(lower(name)) WHERE live=1;
         INSERT INTO t VALUES('Ann',1),('BOB',1),('bob',0);",
    );

    // Predicate present and expression matched → seek.
    let p = plan(
        &mut c,
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE lower(name)='bob' AND live=1",
    );
    assert!(p.contains("USING INDEX ipe"), "expected seek, got: {p}");
    let r = c
        .query("SELECT name FROM t WHERE lower(name)='bob' AND live=1")
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Text("BOB".into()));

    // Missing the partial predicate → must scan (and still be correct: 'BOB' and
    // 'bob' both match, regardless of `live`).
    let p = plan(
        &mut c,
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE lower(name)='bob'",
    );
    assert_eq!(p, "SCAN t", "partial expr index must not be used: {p}");
    let r = c
        .query("SELECT name FROM t WHERE lower(name)='bob'")
        .unwrap();
    assert_eq!(r.rows.len(), 2);
}
