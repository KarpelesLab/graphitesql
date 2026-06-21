//! A dedup set operation (UNION/INTERSECT/EXCEPT) returns rows in sorted order
//! when there is no explicit ORDER BY — SQLite implements the dedup via a sorter.
//! UNION ALL preserves order. Matched to the sqlite3 3.50.4 CLI.

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

fn texts(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|mut r| match r.remove(0) {
            Value::Text(s) => s,
            Value::Null => "<null>".into(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn dedup_compound_is_sorted() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        ints(&c, "SELECT 3 UNION SELECT 1 UNION SELECT 2"),
        vec![1, 2, 3]
    );
    assert_eq!(
        ints(&c, "VALUES(3),(1),(2) EXCEPT VALUES(5)"),
        vec![1, 2, 3]
    );
    assert_eq!(ints(&c, "SELECT 2 INTERSECT SELECT 2"), vec![2]);
    // Text sorts by BINARY: 'A' (0x41) before 'a' (0x61).
    assert_eq!(
        texts(&c, "SELECT 'b' UNION SELECT 'a' UNION SELECT 'A'"),
        vec!["A", "a", "b"]
    );
    // Multi-column lexicographic.
    let r = c
        .query("SELECT 3,'z' UNION SELECT 1,'b' UNION SELECT 1,'a'")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Integer(1), Value::Text("a".into())],
            vec![Value::Integer(1), Value::Text("b".into())],
            vec![Value::Integer(3), Value::Text("z".into())],
        ]
    );
}

#[test]
fn union_all_preserves_order_and_explicit_order_by_wins() {
    let c = Connection::open_memory().unwrap();
    // UNION ALL is not sorted.
    assert_eq!(ints(&c, "SELECT 3 UNION ALL SELECT 1"), vec![3, 1]);
    // An explicit ORDER BY overrides the implicit sort.
    assert_eq!(
        ints(&c, "SELECT 3 UNION SELECT 1 UNION SELECT 2 ORDER BY 1 DESC"),
        vec![3, 2, 1]
    );
}
