//! `Connection::last_insert_rowid()`, `changes()`, and `total_changes()` — the
//! `sqlite3_last_insert_rowid()`/`sqlite3_changes()`/`sqlite3_total_changes()`
//! accessors — match the SQL `last_insert_rowid()`/`changes()`/`total_changes()`
//! functions and SQLite's documented semantics.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn int(conn: &Connection, sql: &str) -> i64 {
    match conn.query(sql).unwrap().rows[0][0] {
        Value::Integer(i) => i,
        ref v => panic!("expected integer, got {v:?}"),
    }
}

#[test]
fn change_and_rowid_accessors() {
    let mut c = Connection::open_memory().unwrap();
    // No inserts yet.
    assert_eq!(c.last_insert_rowid(), 0);
    assert_eq!(c.total_changes(), 0);

    c.execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY, b);")
        .unwrap();
    // DDL does not count as a change.
    assert_eq!(c.total_changes(), 0);

    c.execute("INSERT INTO t VALUES(NULL, 'x')").unwrap();
    assert_eq!(c.last_insert_rowid(), 1);
    assert_eq!(c.changes(), 1);
    assert_eq!(c.total_changes(), 1);
    // The Rust accessor agrees with the SQL function.
    assert_eq!(int(&c, "SELECT last_insert_rowid()"), 1);
    assert_eq!(int(&c, "SELECT changes()"), 1);
    assert_eq!(int(&c, "SELECT total_changes()"), 1);

    // A multi-row insert: last rowid is the last assigned, changes = 3.
    c.execute("INSERT INTO t VALUES(NULL,'a'),(NULL,'b'),(NULL,'c')")
        .unwrap();
    assert_eq!(c.last_insert_rowid(), 4);
    assert_eq!(c.changes(), 3);
    assert_eq!(c.total_changes(), 4);

    // An explicit rowid updates last_insert_rowid.
    c.execute("INSERT INTO t VALUES(100, 'z')").unwrap();
    assert_eq!(c.last_insert_rowid(), 100);
    assert_eq!(c.changes(), 1);

    // UPDATE counts toward changes/total but not last_insert_rowid.
    c.execute("UPDATE t SET b='Y' WHERE a<=4").unwrap();
    assert_eq!(c.changes(), 4);
    assert_eq!(c.last_insert_rowid(), 100);
    assert_eq!(c.total_changes(), 9);

    // A SELECT leaves the counters untouched.
    let _ = c.query("SELECT count(*) FROM t").unwrap();
    assert_eq!(c.changes(), 4);
    assert_eq!(c.last_insert_rowid(), 100);

    // DELETE.
    c.execute("DELETE FROM t WHERE a>=100").unwrap();
    assert_eq!(c.changes(), 1);
    assert_eq!(c.total_changes(), 10);
}
