//! A BEFORE INSERT trigger fires *before* constraint and conflict handling
//! (sqlite's `insert.c`: the trigger program runs ahead of NOT NULL/type/CHECK,
//! the uniqueness/PK resolution, and the FK checks). The observable consequences,
//! each pinned to `sqlite3 3.50.4`:
//!
//! * under `INSERT OR REPLACE`, the trigger still sees the conflicting row (it
//!   has not been deleted yet);
//! * a row later skipped by `OR IGNORE` (for a NOT NULL or UNIQUE violation) has
//!   already run its BEFORE trigger's side effects.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn texts(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.to_string(),
            Value::Integer(i) => i.to_string(),
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

#[test]
fn before_insert_fires_before_replace_deletes_conflict() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v)")
        .unwrap();
    c.execute("CREATE TABLE log(msg)").unwrap();
    c.execute(
        "CREATE TRIGGER bi BEFORE INSERT ON t BEGIN \
         INSERT INTO log VALUES('before:'||(SELECT count(*) FROM t)); END",
    )
    .unwrap();
    c.execute("INSERT INTO t VALUES(1,'a')").unwrap();
    c.execute("INSERT OR REPLACE INTO t VALUES(1,'b')").unwrap();
    // First insert saw an empty table; the REPLACE's BEFORE trigger saw the
    // still-present conflicting row (count 1), because the delete happens after.
    assert_eq!(
        texts(&c, "SELECT msg FROM log"),
        vec!["before:0", "before:1"]
    );
    assert_eq!(texts(&c, "SELECT v FROM t"), vec!["b"]);
}

#[test]
fn before_insert_side_effects_persist_when_or_ignore_skips_not_null() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v NOT NULL)")
        .unwrap();
    c.execute("CREATE TABLE log(msg)").unwrap();
    c.execute("CREATE TRIGGER bi BEFORE INSERT ON t BEGIN INSERT INTO log VALUES('fired'); END")
        .unwrap();
    // The row violates NOT NULL and is skipped by OR IGNORE, but the BEFORE
    // trigger already ran (its side effect persists) — sqlite fires the trigger
    // before the NOT NULL check.
    c.execute("INSERT OR IGNORE INTO t VALUES(1,NULL)").unwrap();
    assert_eq!(c.query("SELECT * FROM t").unwrap().rows.len(), 0);
    assert_eq!(texts(&c, "SELECT msg FROM log"), vec!["fired"]);
}

#[test]
fn before_insert_fires_before_or_ignore_skips_unique() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v UNIQUE)")
        .unwrap();
    c.execute("CREATE TABLE log(msg)").unwrap();
    c.execute(
        "CREATE TRIGGER bi BEFORE INSERT ON t BEGIN \
         INSERT INTO log VALUES('f'||(SELECT count(*) FROM t)); END",
    )
    .unwrap();
    c.execute("INSERT INTO t VALUES(1,5)").unwrap();
    c.execute("INSERT OR IGNORE INTO t VALUES(2,5)").unwrap();
    // The UNIQUE conflict skips the second row, but its BEFORE trigger fired
    // first (seeing count 1).
    assert_eq!(texts(&c, "SELECT msg FROM log"), vec!["f0", "f1"]);
    assert_eq!(texts(&c, "SELECT count(*) FROM t"), vec!["1"]);
}
