//! A numeric `RANGE` offset (`RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING`) only
//! spans a *numeric* ORDER BY value. When the ORDER BY key is TEXT (even
//! numeric-looking text like `'1'`) or a BLOB, sqlite has no `value ± n` span,
//! so the frame collapses to the current peer group — the rows equal to it.
//! graphite used to coerce the key to a float and range over it, over-counting.
//!
//! Every expected value is byte-verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn col_i64(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(n) => *n,
            v => panic!("not integer: {v:?}"),
        })
        .collect()
}

#[test]
fn range_offset_over_text_key_uses_peer_group() {
    let c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(g TEXT, v);
         INSERT INTO t VALUES('a',10),('a',20),('b',30),('b',27),('c',5);",
    )
    .unwrap();
    // Each distinct text is its own peer group: a=30, b=57, c=5.
    assert_eq!(
        col_i64(
            &c,
            "SELECT sum(v) OVER (ORDER BY g RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
             FROM t ORDER BY g, v"
        ),
        vec![30, 30, 57, 57, 5]
    );
}

#[test]
fn range_offset_over_numeric_looking_text_is_not_numeric() {
    let c = Connection::open_memory().unwrap();
    c.execute_batch("CREATE TABLE t(g TEXT, v); INSERT INTO t VALUES('1',10),('2',20),('3',30);")
        .unwrap();
    // '1','2','3' are TEXT, so each is its own peer (not 1.0/2.0/3.0).
    assert_eq!(
        col_i64(
            &c,
            "SELECT sum(v) OVER (ORDER BY g RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
             FROM t ORDER BY g"
        ),
        vec![10, 20, 30]
    );
}

#[test]
fn range_offset_over_blob_key_uses_peer_group() {
    let c = Connection::open_memory().unwrap();
    c.execute_batch("CREATE TABLE t(g, v); INSERT INTO t VALUES(x'01',10),(x'02',20);")
        .unwrap();
    assert_eq!(
        col_i64(
            &c,
            "SELECT sum(v) OVER (ORDER BY g RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t"
        ),
        vec![10, 20]
    );
}

#[test]
fn range_offset_over_numeric_key_still_ranges() {
    // Regression guard: genuine numeric keys keep the value-range semantics.
    let c = Connection::open_memory().unwrap();
    c.execute_batch("CREATE TABLE t(g INT, v); INSERT INTO t VALUES(1,10),(2,20),(3,30);")
        .unwrap();
    assert_eq!(
        col_i64(
            &c,
            "SELECT sum(v) OVER (ORDER BY g RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t"
        ),
        vec![30, 60, 50]
    );
}
