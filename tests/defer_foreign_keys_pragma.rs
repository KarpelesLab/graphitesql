//! `PRAGMA defer_foreign_keys=ON` makes every foreign-key constraint behave as
//! `DEFERRABLE INITIALLY DEFERRED` for the current transaction: violations are
//! checked at COMMIT, not immediately, so a child row may be inserted before its
//! parent as long as the parent exists by commit time. SQLite resets the flag at
//! each COMMIT/ROLLBACK, so it must be re-enabled per transaction. graphite
//! previously ignored the pragma (always echoed 0, enforced immediately).
//!
//! Byte-verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE ch(x REFERENCES p);",
    )
    .unwrap();
    c
}

fn flag(c: &Connection) -> i64 {
    match &c.query("PRAGMA defer_foreign_keys").unwrap().rows[0][0] {
        Value::Integer(n) => *n,
        v => panic!("not integer: {v:?}"),
    }
}

#[test]
fn deferred_child_before_parent_commits() {
    let mut c = setup();
    c.execute_batch(
        "BEGIN;
         PRAGMA defer_foreign_keys=ON;
         INSERT INTO ch VALUES(1);
         INSERT INTO p VALUES(1);
         COMMIT;",
    )
    .unwrap();
    assert_eq!(
        c.query("SELECT x FROM ch").unwrap().rows[0][0],
        Value::Integer(1)
    );
}

#[test]
fn unrepaired_violation_faults_at_commit() {
    let mut c = setup();
    c.execute_batch("BEGIN; PRAGMA defer_foreign_keys=ON; INSERT INTO ch VALUES(1);")
        .unwrap();
    let e = c.execute("COMMIT").unwrap_err().to_string();
    assert!(e.contains("FOREIGN KEY constraint failed"), "got: {e}");
}

#[test]
fn flag_resets_after_commit_and_rollback() {
    let mut c = setup();
    assert_eq!(flag(&c), 0);
    c.execute_batch(
        "BEGIN; PRAGMA defer_foreign_keys=ON; INSERT INTO ch VALUES(1); INSERT INTO p VALUES(1); COMMIT;",
    )
    .unwrap();
    assert_eq!(flag(&c), 0, "cleared after COMMIT");

    c.execute_batch("BEGIN; PRAGMA defer_foreign_keys=ON; ROLLBACK;")
        .unwrap();
    assert_eq!(flag(&c), 0, "cleared after ROLLBACK");
}

#[test]
fn flag_does_not_leak_into_next_transaction() {
    let mut c = setup();
    c.execute_batch("BEGIN; PRAGMA defer_foreign_keys=ON; INSERT INTO ch VALUES(1); ROLLBACK;")
        .unwrap();
    // The next transaction has the default (immediate) enforcement again.
    let e = c
        .execute_batch("BEGIN; INSERT INTO ch VALUES(2);")
        .unwrap_err()
        .to_string();
    assert!(e.contains("FOREIGN KEY constraint failed"), "got: {e}");
}

#[test]
fn immediate_enforcement_without_the_pragma() {
    let mut c = setup();
    let e = c
        .execute_batch("BEGIN; INSERT INTO ch VALUES(1);")
        .unwrap_err()
        .to_string();
    assert!(e.contains("FOREIGN KEY constraint failed"), "got: {e}");
}
