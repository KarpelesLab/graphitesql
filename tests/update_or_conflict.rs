//! `UPDATE OR IGNORE / REPLACE / ABORT` conflict resolution. Verified against
//! sqlite3 semantics.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, u UNIQUE, v INT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(1,10,100),(2,20,200),(3,30,300)")
        .unwrap();
    c
}

fn pairs(c: &Connection) -> Vec<(i64, i64)> {
    c.query("SELECT a, u FROM t ORDER BY a")
        .unwrap()
        .rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Integer(a), Value::Integer(u)) => (*a, *u),
            _ => panic!(),
        })
        .collect()
}

#[test]
fn or_ignore_skips_conflicting_row() {
    let mut c = setup();
    // Setting u=20 collides with row 2; OR IGNORE leaves everything unchanged.
    assert_eq!(
        c.execute("UPDATE OR IGNORE t SET u=20 WHERE a=1").unwrap(),
        0
    );
    assert_eq!(pairs(&c), [(1, 10), (2, 20), (3, 30)]);
}

#[test]
fn or_replace_deletes_conflicting_row() {
    let mut c = setup();
    // Setting u=30 collides with row 3; OR REPLACE deletes row 3, updates row 1.
    c.execute("UPDATE OR REPLACE t SET u=30 WHERE a=1").unwrap();
    assert_eq!(pairs(&c), [(1, 30), (2, 20)]);
}

#[test]
fn or_ignore_skips_not_null_violation() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE n(a INTEGER PRIMARY KEY, b NOT NULL)")
        .unwrap();
    c.execute("INSERT INTO n VALUES(1,'x'),(2,'y')").unwrap();
    c.execute("UPDATE OR IGNORE n SET b=NULL WHERE a=1")
        .unwrap();
    assert_eq!(
        c.query("SELECT b FROM n WHERE a=1").unwrap().rows[0][0],
        Value::Text("x".into())
    );
}

#[test]
fn default_and_or_abort_still_error_on_conflict() {
    let mut c = setup();
    assert!(c.execute("UPDATE t SET u=20 WHERE a=1").is_err());
    assert!(c.execute("UPDATE OR ABORT t SET u=20 WHERE a=1").is_err());
    assert!(
        c.execute("UPDATE OR ROLLBACK t SET u=20 WHERE a=1")
            .is_err()
    );
    // Nothing changed.
    assert_eq!(pairs(&c), [(1, 10), (2, 20), (3, 30)]);
}
