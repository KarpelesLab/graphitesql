//! Constraint-level `ON CONFLICT <action>` resolution.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

#[test]
fn constraint_level_on_conflict_action() {
    // A UNIQUE/PRIMARY KEY constraint's declared `ON CONFLICT <action>` resolves a
    // violating INSERT/UPDATE when the statement has no `OR <action>` of its own,
    // byte-for-byte like sqlite3 (a statement-level `OR` overrides it).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a UNIQUE ON CONFLICT REPLACE, b)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(1,'x')").unwrap();
    c.execute("INSERT INTO t VALUES(1,'y')").unwrap(); // REPLACE, not ABORT
    assert_eq!(
        rows(&c, "SELECT a,b FROM t"),
        [vec![Value::Integer(1), Value::Text("y".into())]]
    );

    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a UNIQUE ON CONFLICT IGNORE, b)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(1,'x')").unwrap();
    c.execute("INSERT INTO t VALUES(1,'y')").unwrap(); // IGNORE: keeps 'x'
    assert_eq!(rows(&c, "SELECT b FROM t"), [vec![Value::Text("x".into())]]);
    // A statement-level OR overrides the constraint's IGNORE.
    c.execute("INSERT OR REPLACE INTO t VALUES(1,'z')").unwrap();
    assert_eq!(rows(&c, "SELECT b FROM t"), [vec![Value::Text("z".into())]]);

    // Table-level UNIQUE(...) ON CONFLICT REPLACE, and the schema text round-trips.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b, UNIQUE(a) ON CONFLICT REPLACE)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(1,'x'),(1,'y')").unwrap();
    assert_eq!(
        rows(&c, "SELECT a,b FROM t"),
        [vec![Value::Integer(1), Value::Text("y".into())]]
    );
    assert_eq!(
        rows(&c, "SELECT sql FROM sqlite_master WHERE type='table'"),
        [vec![Value::Text(
            "CREATE TABLE t(a, b, UNIQUE(a) ON CONFLICT REPLACE)".into()
        )]]
    );
}

#[test]
fn not_null_on_conflict_action() {
    // A NOT NULL column's `ON CONFLICT` action resolves a NULL write: IGNORE skips
    // the row, REPLACE substitutes the column DEFAULT (erroring if there is none),
    // byte-for-byte like sqlite3.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a NOT NULL ON CONFLICT IGNORE, b)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(NULL,'x'),(1,'y')").unwrap(); // NULL row skipped
    assert_eq!(
        rows(&c, "SELECT a,b FROM t"),
        [vec![Value::Integer(1), Value::Text("y".into())]]
    );

    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a NOT NULL ON CONFLICT REPLACE DEFAULT 9, b)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(NULL,'x')").unwrap(); // NULL → DEFAULT 9
    assert_eq!(rows(&c, "SELECT a FROM t"), [vec![Value::Integer(9)]]);
    // UPDATE to NULL likewise substitutes the default.
    c.execute("UPDATE t SET a=NULL").unwrap();
    assert_eq!(rows(&c, "SELECT a FROM t"), [vec![Value::Integer(9)]]);

    // REPLACE without a default still fails (NOT NULL cannot be satisfied).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a NOT NULL ON CONFLICT REPLACE, b)")
        .unwrap();
    assert!(c.execute("INSERT INTO t VALUES(NULL,'x')").is_err());
    // A statement-level OR overrides the constraint's IGNORE.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a NOT NULL ON CONFLICT IGNORE)")
        .unwrap();
    assert!(c.execute("INSERT OR ABORT INTO t VALUES(NULL)").is_err());
}
