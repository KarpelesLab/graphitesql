//! End-to-end query tests: run SQL through `Connection` against real fixtures.

#![cfg(feature = "std")]

use graphitesql::Connection;
use graphitesql::Value;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn conn(name: &str) -> Connection {
    Connection::open_readonly(&fixture(name)).expect("open connection")
}

fn ints(col: &[Vec<Value>], i: usize) -> Vec<i64> {
    col.iter()
        .map(|r| match &r[i] {
            Value::Integer(v) => *v,
            other => panic!("expected integer, got {other:?}"),
        })
        .collect()
}

#[test]
fn select_columns_with_rowid_alias() {
    let c = conn("basic.db");
    let r = c.query("SELECT a, b FROM t").unwrap();
    assert_eq!(r.columns, vec!["a", "b"]);
    // `a` is INTEGER PRIMARY KEY -> aliases the rowid (1,2,3).
    assert_eq!(ints(&r.rows, 0), vec![1, 2, 3]);
    assert_eq!(r.rows[0][1], Value::Text("hello".into()));
    assert_eq!(r.rows[2][1], Value::Null);
}

#[test]
fn where_filter() {
    let c = conn("basic.db");
    let r = c.query("SELECT a FROM t WHERE a > 1").unwrap();
    assert_eq!(ints(&r.rows, 0), vec![2, 3]);

    let r = c.query("SELECT a FROM t WHERE b IS NOT NULL").unwrap();
    assert_eq!(ints(&r.rows, 0), vec![1, 2]);

    let r = c.query("SELECT a FROM t WHERE b LIKE 'wor%'").unwrap();
    assert_eq!(ints(&r.rows, 0), vec![2]);
}

#[test]
fn order_by_and_limit() {
    let c = conn("basic.db");
    let r = c.query("SELECT a FROM t ORDER BY a DESC").unwrap();
    assert_eq!(ints(&r.rows, 0), vec![3, 2, 1]);

    let r = c
        .query("SELECT a FROM t ORDER BY a LIMIT 1 OFFSET 1")
        .unwrap();
    assert_eq!(ints(&r.rows, 0), vec![2]);
}

#[test]
fn expressions_without_from() {
    let c = conn("basic.db");
    let r = c.query("SELECT 1 + 2 * 3").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(7)]]);

    let r = c.query("SELECT 'a' || 'b' || 'c'").unwrap();
    assert_eq!(r.rows[0][0], Value::Text("abc".into()));

    let r = c
        .query("SELECT upper('hi'), length('hello'), abs(-5)")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Text("HI".into()));
    assert_eq!(r.rows[0][1], Value::Integer(5));
    assert_eq!(r.rows[0][2], Value::Integer(5));
}

#[test]
fn scalar_functions_on_rows() {
    let c = conn("basic.db");
    let r = c.query("SELECT upper(b) FROM t WHERE a = 1").unwrap();
    assert_eq!(r.rows[0][0], Value::Text("HELLO".into()));

    let r = c
        .query("SELECT coalesce(b, 'none') FROM t ORDER BY a")
        .unwrap();
    assert_eq!(r.rows[2][0], Value::Text("none".into()));
}

#[test]
fn aggregates_whole_table() {
    let c = conn("basic.db");
    let r = c
        .query("SELECT count(*), sum(a), max(a), min(a) FROM t")
        .unwrap();
    assert_eq!(
        r.rows[0],
        vec![
            Value::Integer(3),
            Value::Integer(6),
            Value::Integer(3),
            Value::Integer(1)
        ]
    );
}

#[test]
fn aggregates_big_table() {
    let c = conn("big.db");
    let r = c
        .query("SELECT count(*), sum(id), max(id), min(id) FROM nums")
        .unwrap();
    assert_eq!(
        r.rows[0],
        vec![
            Value::Integer(2000),
            Value::Integer(2_001_000),
            Value::Integer(2000),
            Value::Integer(1)
        ]
    );

    let r = c
        .query("SELECT count(*) FROM nums WHERE id <= 100")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(100));
}

#[test]
fn group_by() {
    let c = conn("big.db");
    // Group 2000 ids by id % 3; counts must sum back to 2000.
    let r = c
        .query("SELECT id % 3 AS m, count(*) AS n FROM nums GROUP BY id % 3 ORDER BY m")
        .unwrap();
    assert_eq!(r.columns, vec!["m", "n"]);
    let total: i64 = ints(&r.rows, 1).iter().sum();
    assert_eq!(total, 2000);
    // ids 1..=2000: remainders 1,2,0 -> counts 667,667,666 sorted by m=0,1,2.
    assert_eq!(ints(&r.rows, 0), vec![0, 1, 2]);
    assert_eq!(ints(&r.rows, 1), vec![666, 667, 667]);
}

#[test]
fn distinct() {
    let c = conn("big.db");
    let r = c
        .query("SELECT DISTINCT id % 2 FROM nums ORDER BY 1")
        .unwrap();
    assert_eq!(ints(&r.rows, 0), vec![0, 1]);
}

#[test]
fn having() {
    let c = conn("big.db");
    let r = c
        .query("SELECT id % 5 AS m, count(*) AS n FROM nums GROUP BY id % 5 HAVING count(*) > 0 ORDER BY m")
        .unwrap();
    assert_eq!(r.rows.len(), 5);
}

#[test]
fn case_expression() {
    let c = conn("basic.db");
    let r = c
        .query("SELECT CASE WHEN a = 1 THEN 'one' WHEN a = 2 THEN 'two' ELSE 'many' END FROM t ORDER BY a")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Text("one".into()));
    assert_eq!(r.rows[1][0], Value::Text("two".into()));
    assert_eq!(r.rows[2][0], Value::Text("many".into()));
}
