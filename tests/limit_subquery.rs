//! LIMIT / OFFSET may be a subquery or an expression containing one — the limit
//! evaluation context now resolves subqueries. Matched to the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn ints(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|mut r| match r.remove(0) {
            Value::Integer(i) => i,
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn limit_offset_accept_subqueries() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3),(4),(5)")
        .unwrap();
    assert_eq!(
        ints(&c, "SELECT a FROM t ORDER BY a LIMIT (SELECT 3)"),
        vec![1, 2, 3]
    );
    assert_eq!(
        ints(
            &c,
            "SELECT a FROM t ORDER BY a LIMIT (SELECT 2) OFFSET (SELECT 1)"
        ),
        vec![2, 3]
    );
    // An expression built from a (scalar/aggregate) subquery.
    assert_eq!(
        ints(
            &c,
            "SELECT a FROM t ORDER BY a LIMIT (SELECT max(a) FROM t) - 3"
        ),
        vec![1, 2]
    );
    // In a recursive CTE's limit.
    assert_eq!(
        ints(
            &c,
            "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM r WHERE x<10) \
             SELECT x FROM r LIMIT (SELECT 4)"
        ),
        vec![1, 2, 3, 4]
    );
}
