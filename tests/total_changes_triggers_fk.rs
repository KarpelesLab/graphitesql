//! `total_changes()` counts every row inserted/updated/deleted since the
//! connection opened, INCLUDING rows changed by triggers (before/after, nested)
//! and foreign-key actions (cascade delete, SET NULL). `changes()` reports only
//! the outermost statement's direct row count. graphite previously counted only
//! the outer statement in both. Byte-verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn scalar(c: &Connection, sql: &str) -> i64 {
    match &c.query(sql).unwrap().rows[0][0] {
        Value::Integer(n) => *n,
        v => panic!("not integer: {v:?}"),
    }
}

#[test]
fn total_changes_includes_trigger_rows() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(a);
         CREATE TABLE log(x);
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.a); END;",
    )
    .unwrap();
    c.execute("INSERT INTO t VALUES(1),(2)").unwrap();
    // 2 direct + 2 trigger-body inserts.
    assert_eq!(scalar(&c, "SELECT total_changes()"), 4);
    // changes() is only the outer statement's direct rows.
    assert_eq!(scalar(&c, "SELECT changes()"), 2);
}

#[test]
fn total_changes_counts_nested_triggers() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(a); CREATE TABLE u(a); CREATE TABLE log(x);
         CREATE TRIGGER tt AFTER INSERT ON t BEGIN INSERT INTO u VALUES(NEW.a); END;
         CREATE TRIGGER ut AFTER INSERT ON u BEGIN INSERT INTO log VALUES(NEW.a); END;",
    )
    .unwrap();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    assert_eq!(scalar(&c, "SELECT total_changes()"), 3); // t + u + log
}

#[test]
fn total_changes_counts_fk_cascade_delete() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE ch(x REFERENCES p ON DELETE CASCADE);
         INSERT INTO p VALUES(1);
         INSERT INTO ch VALUES(1),(1);",
    )
    .unwrap();
    let before = scalar(&c, "SELECT total_changes()"); // 3 inserts
    c.execute("DELETE FROM p").unwrap();
    // +1 parent delete, +2 cascade child deletes.
    assert_eq!(scalar(&c, "SELECT total_changes()") - before, 3);
}

#[test]
fn total_changes_counts_fk_set_null() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE ch(x REFERENCES p ON DELETE SET NULL);
         INSERT INTO p VALUES(1);
         INSERT INTO ch VALUES(1),(1);",
    )
    .unwrap();
    let before = scalar(&c, "SELECT total_changes()");
    c.execute("DELETE FROM p").unwrap();
    // +1 parent delete, +2 SET NULL child updates.
    assert_eq!(scalar(&c, "SELECT total_changes()") - before, 3);
}
