//! Grouped output (no explicit ORDER BY) comes out ordered by the GROUP BY keys
//! — SQLite groups via a sort. Matched to the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

fn i(n: i64) -> Value {
    Value::Integer(n)
}
fn t(s: &str) -> Value {
    Value::Text(s.into())
}

#[test]
fn grouped_output_is_ordered_by_keys() {
    let c = Connection::open_memory().unwrap();
    // Integer keys ascending.
    assert_eq!(
        rows(
            &c,
            "WITH t(a) AS (VALUES(3),(1),(2),(1),(3)) SELECT a, count(*) FROM t GROUP BY a"
        ),
        vec![vec![i(1), i(2)], vec![i(2), i(1)], vec![i(3), i(2)]]
    );
    // NULL group sorts first.
    assert_eq!(
        rows(
            &c,
            "WITH t(a) AS (VALUES(3),(NULL),(1),(NULL)) SELECT a, count(*) FROM t GROUP BY a"
        ),
        vec![vec![Value::Null, i(2)], vec![i(1), i(1)], vec![i(3), i(1)]]
    );
    // Multi-column lexicographic.
    assert_eq!(
        rows(
            &c,
            "WITH t(a,b) AS (VALUES(2,'y'),(1,'x'),(2,'x'),(1,'y')) \
             SELECT a,b FROM t GROUP BY a,b"
        ),
        vec![
            vec![i(1), t("x")],
            vec![i(1), t("y")],
            vec![i(2), t("x")],
            vec![i(2), t("y")],
        ]
    );
}

#[test]
fn collation_and_explicit_order_by() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT COLLATE NOCASE)").unwrap();
    c.execute("INSERT INTO t VALUES('b'),('A'),('a'),('B')")
        .unwrap();
    // Grouped + ordered under the column's NOCASE collation.
    assert_eq!(
        rows(&c, "SELECT a, count(*) FROM t GROUP BY a"),
        vec![vec![t("A"), i(2)], vec![t("b"), i(2)]]
    );
    // An explicit ORDER BY overrides the implicit key order.
    let mut c2 = Connection::open_memory().unwrap();
    c2.execute("CREATE TABLE u(a)").unwrap();
    c2.execute("INSERT INTO u VALUES(1),(2),(3)").unwrap();
    assert_eq!(
        rows(&c2, "SELECT a FROM u GROUP BY a ORDER BY a DESC"),
        vec![vec![i(3)], vec![i(2)], vec![i(1)]]
    );
}
