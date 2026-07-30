//! `UPDATE OR IGNORE` / `UPDATE OR REPLACE` on a WITHOUT ROWID table honor the
//! statement's conflict policy when the new row collides with another on the PK
//! or a UNIQUE constraint: IGNORE leaves the row unchanged, REPLACE deletes the
//! row(s) it collides with and applies the update. graphite previously always
//! raised "UNIQUE constraint failed" on the WR path regardless of the OR clause.
//! Byte-verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(sql: &str) -> Vec<(i64, i64)> {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(sql).unwrap();
    c.query("SELECT k, v FROM t ORDER BY k")
        .unwrap()
        .rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Integer(k), Value::Integer(v)) => (*k, *v),
            o => panic!("{o:?}"),
        })
        .collect()
}

#[test]
fn or_ignore_unique_conflict_skips_row() {
    let r = rows(
        "CREATE TABLE t(k PRIMARY KEY, v UNIQUE) WITHOUT ROWID;
         INSERT INTO t VALUES(1,10),(2,20);
         UPDATE OR IGNORE t SET v=10 WHERE k=2;",
    );
    assert_eq!(r, [(1, 10), (2, 20)]); // k=2 unchanged
}

#[test]
fn or_ignore_pk_conflict_skips_row() {
    let r = rows(
        "CREATE TABLE t(k PRIMARY KEY, v) WITHOUT ROWID;
         INSERT INTO t VALUES(1,10),(2,20);
         UPDATE OR IGNORE t SET k=1 WHERE k=2;",
    );
    assert_eq!(r, [(1, 10), (2, 20)]);
}

#[test]
fn or_replace_unique_conflict_deletes_other() {
    let r = rows(
        "CREATE TABLE t(k PRIMARY KEY, v UNIQUE) WITHOUT ROWID;
         INSERT INTO t VALUES(1,10),(2,20);
         UPDATE OR REPLACE t SET v=10 WHERE k=2;",
    );
    // k=1 (the row holding v=10) is deleted; k=2 now holds v=10.
    assert_eq!(r, [(2, 10)]);
}

#[test]
fn default_abort_still_errors() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(k PRIMARY KEY, v UNIQUE) WITHOUT ROWID;
         INSERT INTO t VALUES(1,10),(2,20);",
    )
    .unwrap();
    let e = c
        .execute("UPDATE t SET v=10 WHERE k=2")
        .unwrap_err()
        .to_string();
    assert!(e.contains("UNIQUE constraint failed"), "got: {e}");
}

#[test]
fn transient_duplicate_swap_still_rejected() {
    // Swapping two UNIQUE values transiently duplicates mid-statement — rejected
    // even though the final rows would be distinct (matches sqlite's WR path).
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(k PRIMARY KEY, v UNIQUE) WITHOUT ROWID;
         INSERT INTO t VALUES(1,10),(2,20);",
    )
    .unwrap();
    assert!(
        c.execute("UPDATE t SET v = CASE k WHEN 1 THEN 20 WHEN 2 THEN 10 END")
            .is_err()
    );
}

/// A column-declared `UNIQUE`/`PRIMARY KEY ON CONFLICT <action>` applies on the
/// WITHOUT ROWID insert and update paths when the statement has no `OR` clause
/// of its own — matching the rowid path. (The rowid `INTEGER PRIMARY KEY ON
/// CONFLICT …` case is a separate, pre-existing gap, unrelated to WITHOUT ROWID.)
#[test]
fn constraint_declared_on_conflict_insert() {
    // UNIQUE ON CONFLICT IGNORE: the second row's duplicate v is skipped.
    assert_eq!(
        rows(
            "CREATE TABLE t(k PRIMARY KEY, v UNIQUE ON CONFLICT IGNORE) WITHOUT ROWID;
             INSERT INTO t VALUES(1,10);
             INSERT INTO t VALUES(2,10);"
        ),
        [(1, 10)]
    );
    // PRIMARY KEY ON CONFLICT REPLACE: the second insert replaces row k=1.
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(k PRIMARY KEY ON CONFLICT REPLACE, v) WITHOUT ROWID;
         INSERT INTO t VALUES(1,'a');
         INSERT INTO t VALUES(1,'b');",
    )
    .unwrap();
    assert_eq!(
        c.query("SELECT v FROM t").unwrap().rows[0][0],
        Value::Text("b".into())
    );
}

#[test]
fn constraint_declared_on_conflict_update() {
    // UNIQUE ON CONFLICT IGNORE honored by a plain UPDATE (no OR clause).
    assert_eq!(
        rows(
            "CREATE TABLE t(k PRIMARY KEY, v UNIQUE ON CONFLICT IGNORE) WITHOUT ROWID;
             INSERT INTO t VALUES(1,10),(2,20);
             UPDATE t SET v=10 WHERE k=2;"
        ),
        [(1, 10), (2, 20)]
    );
}

#[test]
fn statement_or_clause_overrides_constraint_action() {
    // `INSERT OR REPLACE` overrides the column's ON CONFLICT IGNORE.
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(k PRIMARY KEY, v UNIQUE ON CONFLICT IGNORE) WITHOUT ROWID;
         INSERT INTO t VALUES(1,10);
         INSERT OR REPLACE INTO t VALUES(2,10);",
    )
    .unwrap();
    // Row k=1 (holding v=10) is replaced away; only k=2 remains.
    assert_eq!(
        c.query("SELECT k, v FROM t").unwrap().rows,
        [[Value::Integer(2), Value::Integer(10)]]
    );
}
