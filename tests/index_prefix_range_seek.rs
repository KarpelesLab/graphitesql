//! A composite-index equality prefix followed by a range on the next column
//! (`WHERE x=? AND y>?`) is seeked as one bounded index range — EXPLAIN QUERY
//! PLAN reads `SEARCH … USING INDEX iu (x=? AND y>?)`, matching sqlite3 3.50.4,
//! and the rows are unchanged from the scan path.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn detail(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match r.last() {
            Some(Value::Text(s)) => s.clone(),
            other => panic!("detail not text: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn col0(c: &Connection, sql: &str) -> Vec<Value> {
    c.query(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(x, y, z)").unwrap();
    c.execute("CREATE INDEX iu ON u(x, y)").unwrap();
    c.execute("INSERT INTO u VALUES (1,10,'p'),(1,20,'q'),(1,30,'r'),(2,10,'s'),(2,20,'t')")
        .unwrap();
    c
}

#[test]
fn equality_prefix_with_lower_bound() {
    let c = setup();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT z FROM u WHERE x=1 AND y>15"),
        "SEARCH u USING INDEX iu (x=? AND y>?)"
    );
    assert_eq!(
        col0(&c, "SELECT z FROM u WHERE x=1 AND y>15 ORDER BY z"),
        [Value::Text("q".into()), Value::Text("r".into())]
    );
}

#[test]
fn equality_prefix_with_two_bounds() {
    let c = setup();
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT z FROM u WHERE x=1 AND y>15 AND y<25"
        ),
        "SEARCH u USING INDEX iu (x=? AND y>? AND y<?)"
    );
    assert_eq!(
        col0(&c, "SELECT z FROM u WHERE x=1 AND y>15 AND y<25"),
        [Value::Text("q".into())]
    );
}

#[test]
fn equality_prefix_with_upper_bound_only() {
    let c = setup();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT z FROM u WHERE x=2 AND y<=10"),
        "SEARCH u USING INDEX iu (x=? AND y<?)"
    );
    assert_eq!(
        col0(&c, "SELECT z FROM u WHERE x=2 AND y<=10"),
        [Value::Text("s".into())]
    );
}

#[test]
fn covering_equality_prefix_range() {
    let c = setup();
    // Reading only indexed columns keeps it a covering seek with the same bounds.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT y FROM u WHERE x=1 AND y>15"),
        "SEARCH u USING COVERING INDEX iu (x=? AND y>?)"
    );
    assert_eq!(
        col0(&c, "SELECT y FROM u WHERE x=1 AND y>15 ORDER BY y"),
        [Value::Integer(20), Value::Integer(30)]
    );
}

#[test]
fn descending_second_column() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE d(x, y, z)").unwrap();
    c.execute("CREATE INDEX id ON d(x ASC, y DESC)").unwrap();
    c.execute("INSERT INTO d VALUES (1,10,'a'),(1,20,'b'),(2,5,'c')")
        .unwrap();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT z FROM d WHERE x=1 AND y>5"),
        "SEARCH d USING INDEX id (x=? AND y>?)"
    );
    assert_eq!(
        col0(&c, "SELECT z FROM d WHERE x=1 AND y>5 ORDER BY z"),
        [Value::Text("a".into()), Value::Text("b".into())]
    );
}

#[test]
fn plain_multi_equality_is_unaffected() {
    let c = setup();
    // A pure multi-equality (no trailing range) still renders without a range.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT z FROM u WHERE x=1 AND y=20"),
        "SEARCH u USING INDEX iu (x=? AND y=?)"
    );
    assert_eq!(
        col0(&c, "SELECT z FROM u WHERE x=1 AND y=20"),
        [Value::Text("q".into())]
    );
}
