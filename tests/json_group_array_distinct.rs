//! `json_group_array(DISTINCT x)` dedupes its values, like other DISTINCT
//! aggregates. Matched to the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn t(c: &Connection, sql: &str) -> String {
    match c.query(sql).unwrap().rows.remove(0).remove(0) {
        Value::Text(s) => String::from(s.as_str()),
        other => panic!("expected text from {sql}, got {other:?}"),
    }
}

#[test]
fn json_group_array_distinct() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(2),(3),(1)")
        .unwrap();
    // DISTINCT dedupes, preserving first-seen order.
    assert_eq!(
        t(&c, "SELECT json_group_array(DISTINCT x) FROM t"),
        "[1,2,3]"
    );
    // Without DISTINCT every row is kept.
    assert_eq!(t(&c, "SELECT json_group_array(x) FROM t"), "[1,2,2,3,1]");
    // Works for text too.
    let mut c2 = Connection::open_memory().unwrap();
    c2.execute("CREATE TABLE s(x)").unwrap();
    c2.execute("INSERT INTO s VALUES('a'),('a'),('b')").unwrap();
    assert_eq!(
        t(&c2, "SELECT json_group_array(DISTINCT x) FROM s"),
        r#"["a","b"]"#
    );
    // An empty group is an empty array.
    assert_eq!(
        t(
            &c,
            "SELECT json_group_array(DISTINCT x) FROM t WHERE x > 99"
        ),
        "[]"
    );
}
