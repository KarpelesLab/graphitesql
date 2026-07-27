//! An `ON DELETE`/`ON UPDATE SET NULL` (or `SET DEFAULT` to NULL) that would
//! land a NULL in a NOT NULL child column fails exactly like an ordinary UPDATE:
//! SQLite raises "NOT NULL constraint failed: <table>.<col>". graphite used to
//! store the NULL silently — corrupting the clustered key when the column was
//! part of a WITHOUT ROWID PRIMARY KEY. The whole statement rolls back.
//! Byte-verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup(schema: &str) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(&format!("PRAGMA foreign_keys=ON; {schema}"))
        .unwrap();
    c
}

fn count(c: &Connection, t: &str) -> i64 {
    match &c.query(&format!("SELECT count(*) FROM {t}")).unwrap().rows[0][0] {
        Value::Integer(n) => *n,
        v => panic!("{v:?}"),
    }
}

#[test]
fn rowid_set_null_on_not_null_column_errors_and_rolls_back() {
    let mut c = setup(
        "CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(x INTEGER NOT NULL REFERENCES p ON DELETE SET NULL);
         INSERT INTO p VALUES(1);
         INSERT INTO c VALUES(1);",
    );
    let e = c.execute("DELETE FROM p").unwrap_err().to_string();
    assert!(e.contains("NOT NULL constraint failed: c.x"), "got: {e}");
    // Nothing changed — the parent delete rolled back with the failed action.
    assert_eq!(count(&c, "p"), 1);
    assert_eq!(count(&c, "c"), 1);
}

#[test]
fn without_rowid_set_null_on_pk_column_errors() {
    let mut c = setup(
        "CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(px, v, PRIMARY KEY(px, v),
                        FOREIGN KEY(px) REFERENCES p ON DELETE SET NULL) WITHOUT ROWID;
         INSERT INTO p VALUES(1);
         INSERT INTO c VALUES(1, 'a');",
    );
    let e = c.execute("DELETE FROM p").unwrap_err().to_string();
    assert!(e.contains("NOT NULL constraint failed: c.px"), "got: {e}");
    assert_eq!(count(&c, "c"), 1);
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

#[test]
fn set_null_on_nullable_column_still_works() {
    let mut c = setup(
        "CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(x REFERENCES p ON DELETE SET NULL);
         INSERT INTO p VALUES(1);
         INSERT INTO c VALUES(1);",
    );
    c.execute("DELETE FROM p").unwrap();
    assert_eq!(c.query("SELECT x FROM c").unwrap().rows[0][0], Value::Null);
}
