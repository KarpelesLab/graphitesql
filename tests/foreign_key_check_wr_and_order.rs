//! `PRAGMA foreign_key_check` improvements toward sqlite parity:
//!  * WITHOUT ROWID children are checked (their `rowid` column is NULL), not
//!    silently skipped;
//!  * a child row's violated foreign keys are reported in ascending `fkid`
//!    order (matching `foreign_key_list`'s `id`).
//!
//! Byte-verified against sqlite3 3.50.4. (The cross-table iteration order of the
//! no-argument form follows sqlite's internal hash order and is not modeled; use
//! the single-table form for a deterministic order.)

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

#[test]
fn without_rowid_child_is_checked_with_null_rowid() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE p(id INTEGER PRIMARY KEY);
         CREATE TABLE c(x, y, PRIMARY KEY(x, y), FOREIGN KEY(x) REFERENCES p) WITHOUT ROWID;
         INSERT INTO c VALUES(9, 1), (8, 2);",
    )
    .unwrap();
    // Two orphan rows; rowid column is NULL for a WITHOUT ROWID table.
    let r = rows(&c, "PRAGMA foreign_key_check(c)");
    assert_eq!(r.len(), 2);
    for row in &r {
        assert_eq!(row[0], Value::Text("c".into())); // table
        assert_eq!(row[1], Value::Null); // rowid
        assert_eq!(row[2], Value::Text("p".into())); // parent
        assert_eq!(row[3], Value::Integer(0)); // fkid
    }
}

#[test]
fn violated_fks_reported_in_ascending_fkid_order() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE a(id INTEGER PRIMARY KEY);
         CREATE TABLE b(id INTEGER PRIMARY KEY);
         CREATE TABLE c(x REFERENCES a, y REFERENCES b);
         INSERT INTO c VALUES(1, 2);",
    )
    .unwrap();
    let r = rows(&c, "PRAGMA foreign_key_check(c)");
    // Same child row (rowid 1) violates both FKs: fkid 0 (b) then fkid 1 (a).
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][3], Value::Integer(0));
    assert_eq!(r[0][2], Value::Text("b".into()));
    assert_eq!(r[1][3], Value::Integer(1));
    assert_eq!(r[1][2], Value::Text("a".into()));
}
