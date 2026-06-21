//! A partial index (`CREATE INDEX … WHERE pred`) and an expression index
//! (`CREATE INDEX … (f(x))`) are now used for *range* seeks, not just equality
//! (roadmap A3b). EXPLAIN QUERY PLAN reads `SEARCH … USING INDEX i (… > ?)`,
//! matching sqlite3 3.50.4, and the rows are unchanged from the scan path.

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

fn ints(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref o => panic!("not int: {o:?}"),
        })
        .collect()
}

#[test]
fn partial_index_range_seek() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE INDEX ip ON t(b) WHERE a > 2").unwrap();
    c.execute("INSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40),(5,50)")
        .unwrap();
    // The partial predicate `a>2` is guaranteed by the WHERE, so the range on the
    // indexed column `b` seeks the partial index.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT b FROM t WHERE a>2 AND b>25"),
        "SEARCH t USING INDEX ip (b>?)"
    );
    assert_eq!(
        ints(&c, "SELECT b FROM t WHERE a>2 AND b>25 ORDER BY b"),
        [30, 40, 50]
    );
    // Two-sided bound.
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT b FROM t WHERE a>2 AND b>15 AND b<45"
        ),
        "SEARCH t USING INDEX ip (b>? AND b<?)"
    );
    assert_eq!(
        ints(&c, "SELECT b FROM t WHERE a>2 AND b>15 AND b<45 ORDER BY b"),
        [30, 40]
    );
}

#[test]
fn partial_predicate_not_guaranteed_falls_back_to_scan() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE INDEX ip ON t(b) WHERE a > 2").unwrap();
    c.execute("INSERT INTO t VALUES (1,10),(3,30),(5,50)")
        .unwrap();
    // Without the `a>2` guard the partial index can't be used: a plain scan, but
    // still the right rows.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT b FROM t WHERE b>25"),
        "SCAN t"
    );
    assert_eq!(ints(&c, "SELECT b FROM t WHERE b>25 ORDER BY b"), [30, 50]);
}

#[test]
fn expression_index_range_seek() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(x)").unwrap();
    c.execute("CREATE INDEX ie ON u(abs(x))").unwrap();
    c.execute("INSERT INTO u VALUES (-5),(-1),(3),(7)").unwrap();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT x FROM u WHERE abs(x)>4"),
        "SEARCH u USING INDEX ie (<expr>>?)"
    );
    assert_eq!(
        ints(&c, "SELECT x FROM u WHERE abs(x)>4 ORDER BY x"),
        [-5, 7]
    );
    // Two-sided bound on the indexed expression.
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT x FROM u WHERE abs(x)>=3 AND abs(x)<7"
        ),
        "SEARCH u USING INDEX ie (<expr>>? AND <expr><?)"
    );
    assert_eq!(
        ints(
            &c,
            "SELECT x FROM u WHERE abs(x)>=3 AND abs(x)<7 ORDER BY x"
        ),
        [-5, 3]
    );
}
