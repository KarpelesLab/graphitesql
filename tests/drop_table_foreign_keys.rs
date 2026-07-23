//! With foreign keys enabled, `DROP TABLE` performs an implicit row-by-row
//! DELETE of the table before removing it (SQLite semantics): dropping a parent
//! still referenced by a `RESTRICT`/`NO ACTION` child fails, `ON DELETE
//! CASCADE`/`SET NULL` children are cleaned up, and a purely self-referential
//! table drops freely. graphite previously dropped the table unconditionally,
//! silently orphaning or corrupting the referencing rows.
//!
//! Every case is byte-verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    c
}

fn count(c: &Connection, t: &str) -> i64 {
    match &c.query(&format!("SELECT count(*) FROM {t}")).unwrap().rows[0][0] {
        Value::Integer(n) => *n,
        v => panic!("not integer: {v:?}"),
    }
}

#[test]
fn drop_parent_referenced_by_no_action_child_fails() {
    let mut c = setup();
    c.execute_batch(
        "CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE ch(x REFERENCES p);
         INSERT INTO p VALUES(1);
         INSERT INTO ch VALUES(1);",
    )
    .unwrap();
    let e = c.execute("DROP TABLE p").unwrap_err().to_string();
    assert!(e.contains("FOREIGN KEY constraint failed"), "got: {e}");
    // The parent table (and its row) survives the failed drop.
    assert_eq!(count(&c, "p"), 1);
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

#[test]
fn drop_parent_with_empty_child_succeeds() {
    let mut c = setup();
    c.execute_batch(
        "CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE ch(x REFERENCES p);
         INSERT INTO p VALUES(1);",
    )
    .unwrap();
    c.execute("DROP TABLE p").unwrap();
    assert!(c.query("SELECT * FROM p").is_err(), "p should be gone");
}

#[test]
fn drop_parent_cascades_to_child() {
    let mut c = setup();
    c.execute_batch(
        "CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE ch(x REFERENCES p ON DELETE CASCADE);
         INSERT INTO p VALUES(1);
         INSERT INTO ch VALUES(1),(1);",
    )
    .unwrap();
    c.execute("DROP TABLE p").unwrap();
    assert_eq!(count(&c, "ch"), 0);
}

#[test]
fn drop_parent_sets_child_null() {
    let mut c = setup();
    c.execute_batch(
        "CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE ch(x REFERENCES p ON DELETE SET NULL);
         INSERT INTO p VALUES(1);
         INSERT INTO ch VALUES(1);",
    )
    .unwrap();
    c.execute("DROP TABLE p").unwrap();
    assert_eq!(c.query("SELECT x FROM ch").unwrap().rows[0][0], Value::Null);
}

#[test]
fn drop_self_referential_table_succeeds() {
    let mut c = setup();
    c.execute_batch(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, p REFERENCES t);
         INSERT INTO t VALUES(1,NULL),(2,1);",
    )
    .unwrap();
    c.execute("DROP TABLE t").unwrap();
    assert!(c.query("SELECT * FROM t").is_err(), "t should be gone");
}

#[test]
fn failed_drop_rolls_back_partial_cascade() {
    // ca would cascade-delete, cb would fault: the whole drop must roll back,
    // leaving ca's rows intact and p present.
    let mut c = setup();
    c.execute_batch(
        "CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE ca(x REFERENCES p ON DELETE CASCADE);
         CREATE TABLE cb(y REFERENCES p);
         INSERT INTO p VALUES(1);
         INSERT INTO ca VALUES(1);
         INSERT INTO cb VALUES(1);",
    )
    .unwrap();
    assert!(c.execute("DROP TABLE p").is_err());
    assert_eq!(count(&c, "ca"), 1);
    assert_eq!(count(&c, "cb"), 1);
    assert_eq!(count(&c, "p"), 1);
}
