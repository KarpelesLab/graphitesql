//! A comma join with the join equality in WHERE (`FROM a, b WHERE a.x = b.y`) is
//! planned like the explicit `a JOIN b ON a.x = b.y`: graphite promotes the
//! qualified equality into the join's ON (copying it, so WHERE still enforces it)
//! and then seeks/hashes the inner table. EXPLAIN QUERY PLAN and results match
//! sqlite3 3.50.4; a non-equality or unrelated predicate stays a SCAN.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn details(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| match row.last() {
            Some(Value::Text(s)) => String::from(s.as_str()),
            other => panic!("detail not text: {other:?}"),
        })
        .collect()
}

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

#[test]
fn where_equality_drives_an_automatic_index() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE TABLE u(c, d)").unwrap();
    assert_eq!(
        details(&c, "EXPLAIN QUERY PLAN SELECT * FROM t, u WHERE t.a=u.c"),
        [
            "SCAN t",
            "BLOOM FILTER ON u (c=?)",
            "SEARCH u USING AUTOMATIC COVERING INDEX (c=?)",
        ]
    );
    // An extra non-join predicate doesn't prevent the promotion.
    assert_eq!(
        details(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM t, u WHERE t.a=u.c AND t.b>5"
        ),
        [
            "SCAN t",
            "BLOOM FILTER ON u (c=?)",
            "SEARCH u USING AUTOMATIC COVERING INDEX (c=?)",
        ]
    );
}

#[test]
fn where_equality_uses_a_real_index_when_present() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE TABLE u(c, d)").unwrap();
    c.execute("CREATE INDEX iu ON u(c)").unwrap();
    assert_eq!(
        details(&c, "EXPLAIN QUERY PLAN SELECT * FROM t, u WHERE t.a=u.c"),
        ["SCAN t", "SEARCH u USING INDEX iu (c=?)"]
    );
}

#[test]
fn a_non_equality_predicate_stays_a_scan() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE TABLE u(c, d)").unwrap();
    assert_eq!(
        details(&c, "EXPLAIN QUERY PLAN SELECT * FROM t, u WHERE t.a>u.c"),
        ["SCAN t", "SCAN u"]
    );
}

#[test]
fn results_are_unchanged_by_the_promotion() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE TABLE u(c, d)").unwrap();
    c.execute("INSERT INTO t VALUES (1,10),(2,20),(3,30)")
        .unwrap();
    c.execute("INSERT INTO u VALUES (1,100),(3,300),(3,301)")
        .unwrap();
    // The join produces exactly the matching pairs (row 3 matches twice).
    assert_eq!(
        rows(
            &c,
            "SELECT t.a, u.d FROM t, u WHERE t.a=u.c ORDER BY t.a, u.d"
        ),
        [
            vec![Value::Integer(1), Value::Integer(100)],
            vec![Value::Integer(3), Value::Integer(300)],
            vec![Value::Integer(3), Value::Integer(301)],
        ]
    );
    // A self comma-join works too (table aliases).
    assert_eq!(
        rows(
            &c,
            "SELECT x.a FROM t x, t y WHERE x.a = y.b / 10 ORDER BY x.a"
        ),
        [
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );
}
