//! Phase 9: non-recursive common table expressions (`WITH`).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT)")
        .unwrap();
    c.execute("INSERT INTO t(a) VALUES (5), (15), (25), (35)")
        .unwrap();
    c
}

#[test]
fn with_clause_as_source() {
    let c = setup();
    let r = c
        .query("WITH big AS (SELECT a FROM t WHERE a > 10) SELECT count(*), sum(a) FROM big")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(3));
    assert_eq!(r.rows[0][1], Value::Integer(75)); // 15+25+35

    let r = c
        .query(
            "WITH big AS (SELECT a FROM t WHERE a > 10) SELECT a FROM big WHERE a < 30 ORDER BY a",
        )
        .unwrap();
    let got: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![15, 25]);
}

#[test]
fn with_explicit_columns() {
    let c = setup();
    let r = c
        .query("WITH r(x) AS (SELECT a FROM t) SELECT x FROM r ORDER BY x LIMIT 1")
        .unwrap();
    assert_eq!(r.columns, vec!["x"]);
    assert_eq!(r.rows[0][0], Value::Integer(5));
}
