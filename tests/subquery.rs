//! Phase 9: scalar subqueries and `IN (SELECT …)` (uncorrelated).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT)")
        .unwrap();
    c.execute("INSERT INTO t(a) VALUES (10), (20), (30), (40)")
        .unwrap();
    c.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, a INT)")
        .unwrap();
    c.execute("INSERT INTO u(a) VALUES (20), (40)").unwrap();
    c
}

#[test]
fn in_select() {
    let c = setup();
    let r = c
        .query("SELECT a FROM t WHERE a IN (SELECT a FROM u) ORDER BY a")
        .unwrap();
    let got: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![20, 40]);

    let r = c
        .query("SELECT count(*) FROM t WHERE a NOT IN (SELECT a FROM u)")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(2)); // 10 and 30
}

#[test]
fn scalar_subquery() {
    let c = setup();
    // As a standalone value.
    let r = c.query("SELECT (SELECT max(a) FROM t)").unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(40));

    // In a projection and in a predicate.
    let r = c
        .query("SELECT a, (SELECT count(*) FROM u) AS uc FROM t WHERE a = 30")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(30));
    assert_eq!(r.rows[0][1], Value::Integer(2));

    let r = c
        .query("SELECT a FROM t WHERE a > (SELECT min(a) FROM u) ORDER BY a")
        .unwrap();
    let got: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![30, 40]); // a > 20
}

#[test]
fn scalar_subquery_no_rows_is_null() {
    let c = setup();
    let r = c.query("SELECT (SELECT a FROM u WHERE a = 999)").unwrap();
    assert_eq!(r.rows[0][0], Value::Null);
}
