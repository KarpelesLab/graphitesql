//! CTE `AS [NOT] MATERIALIZED` hints parse and run (the hint is an optimizer
//! directive that graphite accepts and ignores). Matched to the sqlite3 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn one(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows.remove(0).remove(0)
}

#[test]
fn materialized_hints_parse_and_run() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        one(&c, "WITH t AS MATERIALIZED (SELECT 1) SELECT * FROM t"),
        Value::Integer(1)
    );
    assert_eq!(
        one(&c, "WITH t AS NOT MATERIALIZED (SELECT 1) SELECT * FROM t"),
        Value::Integer(1)
    );
    assert_eq!(
        one(&c, "WITH t(x) AS MATERIALIZED (SELECT 5) SELECT x*2 FROM t"),
        Value::Integer(10)
    );
    // With RECURSIVE and a column list.
    assert_eq!(
        one(
            &c,
            "WITH RECURSIVE c(x) AS NOT MATERIALIZED \
             (VALUES(1) UNION ALL SELECT x+1 FROM c WHERE x<3) SELECT sum(x) FROM c"
        ),
        Value::Integer(6)
    );
    // Multiple CTEs, only one hinted.
    assert_eq!(
        one(
            &c,
            "WITH a AS (SELECT 1 v), b AS MATERIALIZED (SELECT 2 v) SELECT a.v+b.v FROM a,b"
        ),
        Value::Integer(3)
    );
}

#[test]
fn plain_cte_and_not_in_body_unaffected() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        one(&c, "WITH t AS (SELECT 1) SELECT * FROM t"),
        Value::Integer(1)
    );
    // A `NOT` inside the CTE body is not mistaken for the MATERIALIZED hint.
    assert_eq!(
        one(&c, "WITH t AS (SELECT NOT 0) SELECT * FROM t"),
        Value::Integer(1)
    );
}
