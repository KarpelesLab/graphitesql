//! B0b-iii: a `WHERE` seek that walks an index also satisfies an `ORDER BY` on
//! the index columns, so the sort is skipped. For an equality seek the ordered
//! columns follow the equality prefix (`WHERE a=? ORDER BY b`); for a leading-
//! column range seek they are the index columns themselves (`WHERE a>? ORDER BY
//! a, b`). EXPLAIN QUERY PLAN reads `SEARCH t USING INDEX i (…)` with no temp
//! b-tree, matching sqlite3 3.50.4, and the rows are correctly ordered. The
//! optimization is conservative — it fires only when exactly one secondary index
//! can seek, so the executor's choice is unambiguous.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn plan(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match r.last() {
            Some(Value::Text(s)) => s.clone(),
            other => panic!("detail not text: {other:?}"),
        })
        .collect()
}

fn texts(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        })
        .collect()
}

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(a, b, c)").unwrap();
    c.execute("CREATE INDEX iu ON u(a, b)").unwrap();
    c.execute("INSERT INTO u VALUES (1,3,'x'),(1,1,'w'),(1,2,'y'),(2,1,'z')")
        .unwrap();
    c
}

#[test]
fn equality_seek_orders_by_following_column() {
    let c = setup();
    assert_eq!(
        plan(
            &c,
            "EXPLAIN QUERY PLAN SELECT c FROM u WHERE a=1 ORDER BY b"
        ),
        ["SEARCH u USING INDEX iu (a=?)"]
    );
    assert_eq!(
        texts(&c, "SELECT c FROM u WHERE a=1 ORDER BY b"),
        ["w", "y", "x"]
    );
}

#[test]
fn descending_reverses_the_seek_walk() {
    let c = setup();
    assert_eq!(
        plan(
            &c,
            "EXPLAIN QUERY PLAN SELECT c FROM u WHERE a=1 ORDER BY b DESC"
        ),
        ["SEARCH u USING INDEX iu (a=?)"]
    );
    assert_eq!(
        texts(&c, "SELECT c FROM u WHERE a=1 ORDER BY b DESC"),
        ["x", "y", "w"]
    );
}

#[test]
fn covering_equality_seek_orders_too() {
    let c = setup();
    assert_eq!(
        plan(
            &c,
            "EXPLAIN QUERY PLAN SELECT b FROM u WHERE a=1 ORDER BY b"
        ),
        ["SEARCH u USING COVERING INDEX iu (a=?)"]
    );
}

#[test]
fn range_seek_orders_by_the_index_columns() {
    // B0b-iii range case: a leading-column range seek walks the index in key
    // order, so `ORDER BY a, b` (the index columns) needs no sort.
    let c = setup();
    assert_eq!(
        plan(
            &c,
            "EXPLAIN QUERY PLAN SELECT c FROM u WHERE a > 0 ORDER BY a, b"
        ),
        ["SEARCH u USING INDEX iu (a>?)"]
    );
    assert_eq!(
        texts(&c, "SELECT c FROM u WHERE a > 0 ORDER BY a, b"),
        ["w", "y", "x", "z"]
    );
    // Ordering by just the leading range column is satisfied too.
    assert_eq!(
        plan(
            &c,
            "EXPLAIN QUERY PLAN SELECT c FROM u WHERE a > 0 ORDER BY a"
        ),
        ["SEARCH u USING INDEX iu (a>?)"]
    );
}

#[test]
fn range_seek_descending_reverses_the_walk() {
    let c = setup();
    assert_eq!(
        plan(
            &c,
            "EXPLAIN QUERY PLAN SELECT c FROM u WHERE a > 0 ORDER BY a DESC, b DESC"
        ),
        ["SEARCH u USING INDEX iu (a>?)"]
    );
    assert_eq!(
        texts(&c, "SELECT c FROM u WHERE a > 0 ORDER BY a DESC, b DESC"),
        ["z", "x", "y", "w"]
    );
}

#[test]
fn range_seek_with_wrong_order_still_sorts() {
    // Ordering by a column that the range-walk does not produce in order (here
    // `b` alone, across different `a` values) must still sort.
    let c = setup();
    let p = plan(
        &c,
        "EXPLAIN QUERY PLAN SELECT c FROM u WHERE a > 0 ORDER BY b",
    );
    assert!(
        p.iter().any(|s| s.contains("USE TEMP B-TREE")),
        "expected a sort, got {p:?}"
    );
}

#[test]
fn ambiguous_index_choice_falls_back_to_a_sort() {
    let mut c = setup();
    // A second index whose leading column `a` is also eq-seekable makes the
    // executor's choice ambiguous, so the conservative optimization stands down
    // and sorts (results still correct).
    c.execute("CREATE INDEX iu2 ON u(a, c)").unwrap();
    let p = plan(
        &c,
        "EXPLAIN QUERY PLAN SELECT c FROM u WHERE a=1 ORDER BY b",
    );
    assert!(
        p.iter().any(|s| s.contains("USE TEMP B-TREE")),
        "expected a sort, got {p:?}"
    );
    assert_eq!(
        texts(&c, "SELECT c FROM u WHERE a=1 ORDER BY b"),
        ["w", "y", "x"]
    );
}
