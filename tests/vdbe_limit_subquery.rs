//! B5c (VDBE depth): a non-correlated scalar subquery in LIMIT/OFFSET is folded
//! to its constant by the VDBE router's fold pre-pass, so the query runs on the
//! VDBE (`query_vdbe` forces it and errors on fallback) instead of deferring.
#![cfg(feature = "std")]
use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3),(4),(5)")
        .unwrap();
    c.execute("CREATE TABLE u(b)").unwrap();
    c.execute("INSERT INTO u VALUES(9),(9)").unwrap();
    c
}

fn col(c: &Connection, sql: &str) -> Vec<Value> {
    c.query_vdbe(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
}

#[test]
fn limit_offset_scalar_subquery_runs_on_vdbe() {
    let c = setup();
    // LIMIT (SELECT count(*) FROM u) == LIMIT 2
    assert_eq!(
        col(
            &c,
            "SELECT a FROM t ORDER BY a LIMIT (SELECT count(*) FROM u)"
        ),
        vec![Value::Integer(1), Value::Integer(2)]
    );
    // LIMIT (SELECT 2) OFFSET (SELECT 1)
    assert_eq!(
        col(
            &c,
            "SELECT a FROM t ORDER BY a LIMIT (SELECT 2) OFFSET (SELECT 1)"
        ),
        vec![Value::Integer(2), Value::Integer(3)]
    );
    // arithmetic + subquery mix
    assert_eq!(
        col(&c, "SELECT a FROM t ORDER BY a LIMIT (SELECT 1)+1"),
        vec![Value::Integer(1), Value::Integer(2)]
    );
}
