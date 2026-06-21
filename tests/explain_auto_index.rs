//! EXPLAIN QUERY PLAN reports an INNER/LEFT equi-join on an otherwise-unindexed
//! inner table as a BLOOM FILTER plus a SEARCH … USING AUTOMATIC COVERING INDEX,
//! matching sqlite3 3.50.4 — graphite already runs that join with a transient
//! hash index, so the plan now reflects it. Non-equi joins, and joins whose
//! equality lives in WHERE rather than ON, stay a plain SCAN (graphite
//! nested-loops those).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn details(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| match row.last() {
            Some(Value::Text(s)) => s.clone(),
            other => panic!("detail not text: {other:?}"),
        })
        .collect()
}

#[test]
fn unindexed_equi_join_uses_an_automatic_index() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE TABLE u(c, d)").unwrap();
    assert_eq!(
        details(&c, "EXPLAIN QUERY PLAN SELECT * FROM t JOIN u ON t.a=u.c"),
        [
            "SCAN t",
            "BLOOM FILTER ON u (c=?)",
            "SEARCH u USING AUTOMATIC COVERING INDEX (c=?)",
        ]
    );
}

#[test]
fn left_join_marks_the_automatic_index_search() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE TABLE u(c, d)").unwrap();
    assert_eq!(
        details(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM t LEFT JOIN u ON t.a=u.c"
        ),
        [
            "SCAN t",
            "BLOOM FILTER ON u (c=?)",
            "SEARCH u USING AUTOMATIC COVERING INDEX (c=?) LEFT-JOIN",
        ]
    );
}

#[test]
fn each_unindexed_inner_table_gets_its_own_automatic_index() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE TABLE u(c, d)").unwrap();
    c.execute("CREATE TABLE w(e, f)").unwrap();
    assert_eq!(
        details(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM t JOIN u ON t.a=u.c JOIN w ON u.d=w.e"
        ),
        [
            "SCAN t",
            "BLOOM FILTER ON u (c=?)",
            "SEARCH u USING AUTOMATIC COVERING INDEX (c=?)",
            "BLOOM FILTER ON w (e=?)",
            "SEARCH w USING AUTOMATIC COVERING INDEX (e=?)",
        ]
    );
}

#[test]
fn a_real_index_or_a_non_equi_join_is_not_an_automatic_index() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE TABLE u(c, d)").unwrap();

    // A real index on the inner column is used instead.
    c.execute("CREATE INDEX iu ON u(c)").unwrap();
    assert_eq!(
        details(&c, "EXPLAIN QUERY PLAN SELECT * FROM t JOIN u ON t.a=u.c"),
        ["SCAN t", "SEARCH u USING INDEX iu (c=?)"]
    );

    // A non-equality join condition stays a nested-loop SCAN.
    c.execute("CREATE TABLE v(g, h)").unwrap();
    assert_eq!(
        details(&c, "EXPLAIN QUERY PLAN SELECT * FROM t JOIN v ON t.a<v.g"),
        ["SCAN t", "SCAN v"]
    );
}
