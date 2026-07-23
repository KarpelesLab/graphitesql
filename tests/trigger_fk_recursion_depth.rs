//! A runaway recursive trigger, or a self-referential `ON DELETE CASCADE`
//! chain, must fail cleanly with SQLite's "too many levels of trigger
//! recursion" — never overflow the native stack and abort the process.
//!
//! graphitesql runs triggers and foreign-key actions by native recursion
//! through the tree-walking evaluator (SQLite uses a heap-backed bytecode VM),
//! so an unbounded cascade would previously crash. Both paths are now bounded
//! by `MAX_TRIGGER_DEPTH`, matching SQLite's error message. Bounded/shallow
//! cascades — the realistic case — still run to completion.

#![cfg(feature = "std")]

use graphitesql::Connection;

fn err_of(sql: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    match c.execute_batch(sql) {
        Ok(()) => panic!("expected an error, statement succeeded"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn runaway_recursive_trigger_errors_not_crashes() {
    let e = err_of(
        "PRAGMA recursive_triggers=ON;
         CREATE TABLE t(a INTEGER PRIMARY KEY);
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO t VALUES(NEW.a+1); END;
         INSERT INTO t VALUES(1);",
    );
    assert!(
        e.contains("too many levels of trigger recursion"),
        "unexpected error: {e}"
    );
}

#[test]
fn runaway_fk_cascade_errors_not_crashes() {
    // A self-referential ON DELETE CASCADE chain: deleting the root would
    // cascade down the whole chain, recursing once per link.
    let e = err_of(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE t(id INTEGER PRIMARY KEY, parent REFERENCES t ON DELETE CASCADE);
         INSERT INTO t VALUES(1,NULL);
         WITH RECURSIVE s(i) AS (SELECT 2 UNION ALL SELECT i+1 FROM s WHERE i<600)
           INSERT INTO t SELECT i, i-1 FROM s;
         DELETE FROM t WHERE id=1;",
    );
    assert!(
        e.contains("too many levels of trigger recursion"),
        "unexpected error: {e}"
    );
}

#[test]
fn bounded_recursive_trigger_still_runs() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "PRAGMA recursive_triggers=ON;
         CREATE TABLE t(a INTEGER PRIMARY KEY);
         CREATE TRIGGER tr AFTER INSERT ON t WHEN NEW.a<5
           BEGIN INSERT INTO t VALUES(NEW.a+1); END;
         INSERT INTO t VALUES(1);",
    )
    .unwrap();
    let n = c.query("SELECT count(*) FROM t").unwrap().rows[0][0].clone();
    assert_eq!(n, graphitesql::Value::Integer(5));
}

#[test]
fn bounded_fk_cascade_still_runs() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE t(id INTEGER PRIMARY KEY, parent REFERENCES t ON DELETE CASCADE);
         INSERT INTO t VALUES(1,NULL),(2,1),(3,2),(4,3);
         DELETE FROM t WHERE id=1;",
    )
    .unwrap();
    let n = c.query("SELECT count(*) FROM t").unwrap().rows[0][0].clone();
    assert_eq!(n, graphitesql::Value::Integer(0));
}
