//! A user-registered aggregate can be used as a window function (`agg(x) OVER
//! (...)`). graphitesql drives it by recomputing over each frame with a fresh
//! accumulator, so no `xValue`/`xInverse` inverse protocol is required. This is
//! the core half of `sqlite3_create_window_function`.
//!
//! Cross-checked against a built-in: a custom `mysum` must match `sum` over the
//! same window (the sqlite3 CLI can't register aggregates, so the built-in is the
//! oracle here).

#![cfg(feature = "std")]

use graphitesql::{AggregateFunction, Connection, Result, Value};

struct Sum {
    acc: i64,
}
impl AggregateFunction for Sum {
    fn step(&mut self, args: &[Value]) -> Result<()> {
        if let Value::Integer(i) = args[0] {
            self.acc += i;
        }
        Ok(())
    }
    fn finalize(&mut self) -> Result<Value> {
        Ok(Value::Integer(self.acc))
    }
}

fn ints(conn: &Connection, sql: &str) -> Vec<i64> {
    conn.query(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match r.last().unwrap() {
            Value::Integer(i) => *i,
            other => panic!("non-integer result: {other:?}"),
        })
        .collect()
}

fn setup() -> Connection {
    let mut conn = Connection::open_memory().unwrap();
    conn.register_aggregate_function("mysum", || Box::new(Sum { acc: 0 }));
    conn.execute("CREATE TABLE t(id INTEGER, v INTEGER)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40)")
        .unwrap();
    conn
}

#[test]
fn running_window_matches_builtin() {
    let conn = setup();
    let custom = ints(
        &conn,
        "SELECT mysum(v) OVER (ORDER BY id) FROM t ORDER BY id",
    );
    let builtin = ints(&conn, "SELECT sum(v) OVER (ORDER BY id) FROM t ORDER BY id");
    assert_eq!(custom, builtin);
    assert_eq!(custom, [10, 30, 60, 100]);
}

#[test]
fn whole_partition_window() {
    let conn = setup();
    let got = ints(&conn, "SELECT mysum(v) OVER () FROM t");
    assert_eq!(got, [100, 100, 100, 100]);
}

#[test]
fn partition_by_window_matches_builtin() {
    let conn = setup();
    let custom = ints(
        &conn,
        "SELECT mysum(v) OVER (PARTITION BY id%2 ORDER BY id) FROM t ORDER BY id",
    );
    let builtin = ints(
        &conn,
        "SELECT sum(v) OVER (PARTITION BY id%2 ORDER BY id) FROM t ORDER BY id",
    );
    assert_eq!(custom, builtin);
    // odd ids (1,3): 10, 40 ; even ids (2,4): 20, 60 — interleaved by id order.
    assert_eq!(custom, [10, 20, 40, 60]);
}

#[test]
fn explicit_rows_frame_matches_builtin() {
    let conn = setup();
    let frame = "ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW";
    let custom = ints(
        &conn,
        &format!("SELECT mysum(v) OVER ({frame}) FROM t ORDER BY id"),
    );
    let builtin = ints(
        &conn,
        &format!("SELECT sum(v) OVER ({frame}) FROM t ORDER BY id"),
    );
    assert_eq!(custom, builtin);
    assert_eq!(custom, [10, 30, 50, 70]);
}
