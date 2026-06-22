//! UPDATE SET assignments are simultaneous (every RHS evaluated against the
//! original row), and the row-value column-list form `(a,b) = (e1,e2)` parses.
//! Matched to the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn row(c: &Connection, sql: &str) -> Vec<Value> {
    c.query(sql).unwrap().rows.remove(0)
}

fn i(n: i64) -> Value {
    Value::Integer(n)
}

#[test]
fn assignments_are_simultaneous() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a,b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,2)").unwrap();
    // A swap: both RHS see the original values.
    c.execute("UPDATE t SET a=b, b=a").unwrap();
    assert_eq!(row(&c, "SELECT a,b FROM t"), vec![i(2), i(1)]);

    // Cross-references use the original row, not a partially-updated one.
    let mut c2 = Connection::open_memory().unwrap();
    c2.execute("CREATE TABLE t(a,c)").unwrap();
    c2.execute("INSERT INTO t VALUES(1,3)").unwrap();
    c2.execute("UPDATE t SET a=c*10, c=a*10").unwrap();
    assert_eq!(row(&c2, "SELECT a,c FROM t"), vec![i(30), i(10)]);

    // WITHOUT ROWID tables too.
    let mut c3 = Connection::open_memory().unwrap();
    c3.execute("CREATE TABLE t(a PRIMARY KEY, b) WITHOUT ROWID")
        .unwrap();
    c3.execute("INSERT INTO t VALUES(1,2)").unwrap();
    c3.execute("UPDATE t SET b=a, a=b").unwrap();
    assert_eq!(row(&c3, "SELECT a,b FROM t"), vec![i(2), i(1)]);
}

#[test]
fn row_value_column_list_assignment() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a,b,c)").unwrap();
    c.execute("INSERT INTO t VALUES(1,2,3)").unwrap();
    // (a,c) = (c*10, a*10): the i-th column gets the i-th expression, evaluated
    // against the original row.
    c.execute("UPDATE t SET (a,c)=(c*10, a*10)").unwrap();
    assert_eq!(row(&c, "SELECT a,b,c FROM t"), vec![i(30), i(2), i(10)]);

    // Mixed with a plain assignment, and a WHERE.
    c.execute("UPDATE t SET b=99, (a,c)=(0,1) WHERE a=30")
        .unwrap();
    assert_eq!(row(&c, "SELECT a,b,c FROM t"), vec![i(0), i(99), i(1)]);

    // A length mismatch is rejected.
    assert!(c.execute("UPDATE t SET (a,b)=(1,2,3)").is_err());
}

#[test]
fn set_subquery_sees_pre_update_snapshot() {
    // A subquery in a SET expression sees the table as it was BEFORE the UPDATE
    // started, for every row — not rows updated earlier in the same statement —
    // matching sqlite3.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,10),(2,20)").unwrap();
    c.execute("UPDATE t SET b=(SELECT sum(b) FROM t)").unwrap();
    // Both rows use the original sum (30), not 30 then 30+20.
    assert_eq!(c.query("SELECT b FROM t").unwrap().rows, [[i(30)], [i(30)]]);

    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3)").unwrap();
    c.execute("UPDATE t SET a=(SELECT count(*) FROM t)")
        .unwrap();
    assert_eq!(
        c.query("SELECT a FROM t").unwrap().rows,
        [[i(3)], [i(3)], [i(3)]]
    );
}
