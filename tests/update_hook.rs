//! `Connection::register_update_hook` — the data-change notification callback
//! (the core of `sqlite3_update_hook`). It fires once per inserted/updated/
//! deleted row with the operation, table, and rowid.

#![cfg(feature = "std")]

use std::cell::RefCell;
use std::rc::Rc;

use graphitesql::{Connection, UpdateOp};

type Log = Rc<RefCell<Vec<(UpdateOp, String, i64)>>>;

fn with_hook() -> (Connection, Log) {
    let conn = Connection::open_memory().unwrap();
    let log: Log = Rc::new(RefCell::new(Vec::new()));
    let sink = log.clone();
    conn.register_update_hook(move |op, _db, table, rowid| {
        sink.borrow_mut().push((op, table.to_string(), rowid));
    });
    (conn, log)
}

#[test]
fn insert_update_delete_are_reported() {
    let (mut conn, log) = with_hook();
    conn.execute("CREATE TABLE t(a)").unwrap();
    conn.execute("INSERT INTO t VALUES (10),(20)").unwrap();
    conn.execute("UPDATE t SET a=a+1 WHERE a=10").unwrap();
    conn.execute("DELETE FROM t WHERE a=20").unwrap();

    let events = log.borrow().clone();
    assert_eq!(
        events,
        vec![
            (UpdateOp::Insert, "t".to_string(), 1),
            (UpdateOp::Insert, "t".to_string(), 2),
            (UpdateOp::Update, "t".to_string(), 1),
            (UpdateOp::Delete, "t".to_string(), 2),
        ]
    );
}

#[test]
fn remove_hook_stops_notifications() {
    let (mut conn, log) = with_hook();
    conn.execute("CREATE TABLE t(a)").unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();
    assert_eq!(log.borrow().len(), 1);
    conn.remove_update_hook();
    conn.execute("INSERT INTO t VALUES (2)").unwrap();
    assert_eq!(log.borrow().len(), 1, "no events after removal");
}

#[test]
fn ddl_does_not_fire_the_hook() {
    // Only row changes to user tables are reported, not DDL.
    let (mut conn, log) = with_hook();
    conn.execute("CREATE TABLE t(a)").unwrap();
    conn.execute("CREATE INDEX i ON t(a)").unwrap();
    assert!(log.borrow().is_empty());
}
