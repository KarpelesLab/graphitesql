//! An `INTEGER PRIMARY KEY ON CONFLICT <action>` (the rowid alias) applies its
//! declared action when a rowid duplicate is the *sole* conflict and the
//! statement carries no `OR <action>` of its own — matching sqlite. graphite
//! previously ignored the IPK's declared action (always ABORT).
//!
//! Only the sole-conflict case is handled: when the row also collides on another
//! constraint, sqlite's multi-constraint precedence is order-based and not
//! modelled (a documented residual), so those cases are intentionally excluded.
//!
//! Byte-verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn one(sql: &str) -> Value {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(sql).unwrap();
    c.query("SELECT v FROM t").unwrap().rows[0][0].clone()
}

#[test]
fn ipk_on_conflict_replace() {
    // The second insert replaces the row with the same rowid.
    assert_eq!(
        one(
            "CREATE TABLE t(k INTEGER PRIMARY KEY ON CONFLICT REPLACE, v);
             INSERT INTO t VALUES(1,'a');
             INSERT INTO t VALUES(1,'b');"
        ),
        Value::Text("b".into())
    );
}

#[test]
fn ipk_on_conflict_ignore() {
    // The second insert is ignored; the original row stays.
    assert_eq!(
        one(
            "CREATE TABLE t(k INTEGER PRIMARY KEY ON CONFLICT IGNORE, v);
             INSERT INTO t VALUES(1,'a');
             INSERT INTO t VALUES(1,'b');"
        ),
        Value::Text("a".into())
    );
}

#[test]
fn table_level_ipk_on_conflict_replace() {
    assert_eq!(
        one(
            "CREATE TABLE t(k INTEGER, v, PRIMARY KEY(k) ON CONFLICT REPLACE);
             INSERT INTO t VALUES(1,'a');
             INSERT INTO t VALUES(1,'b');"
        ),
        Value::Text("b".into())
    );
}

#[test]
fn ipk_on_conflict_replace_via_update() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(k INTEGER PRIMARY KEY ON CONFLICT REPLACE, v);
         INSERT INTO t VALUES(1,'a'),(2,'b');
         UPDATE t SET k=1 WHERE k=2;",
    )
    .unwrap();
    // k=2's update to k=1 replaces the original k=1 row.
    assert_eq!(
        c.query("SELECT k, v FROM t").unwrap().rows,
        [[Value::Integer(1), Value::Text("b".into())]]
    );
}

#[test]
fn default_ipk_still_aborts() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch("CREATE TABLE t(k INTEGER PRIMARY KEY, v); INSERT INTO t VALUES(1,'a');")
        .unwrap();
    let e = c
        .execute("INSERT INTO t VALUES(1,'b')")
        .unwrap_err()
        .to_string();
    assert!(e.contains("UNIQUE constraint failed: t.k"), "got: {e}");
}

#[test]
fn statement_or_clause_overrides_ipk_action() {
    // A statement `OR REPLACE` overrides the IPK's declared ON CONFLICT IGNORE.
    assert_eq!(
        one(
            "CREATE TABLE t(k INTEGER PRIMARY KEY ON CONFLICT IGNORE, v);
             INSERT OR REPLACE INTO t VALUES(1,'a');
             INSERT OR REPLACE INTO t VALUES(1,'b');"
        ),
        Value::Text("b".into())
    );
}
