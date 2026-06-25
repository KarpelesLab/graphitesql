//! An INTEGER PRIMARY KEY *is* the rowid, so a non-integer value is a `datatype
//! mismatch` in SQLite — never a silent `to_i64` coercion. Integer-valued reals
//! and numeric text (2.0, '5', '5.0') are coerced by column affinity and accepted;
//! 1.5, 'x', a blob, and (on UPDATE) NULL are rejected. A plain `INT PRIMARY KEY`
//! is NOT a rowid alias and stays lax. Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn err(c: &mut Connection, sql: &str) -> String {
    c.execute(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .trim_start_matches("Error: ")
        .to_string()
}

#[test]
fn insert_non_integer_into_integer_pk_is_datatype_mismatch() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY)").unwrap();
    for bad in ["INSERT INTO t VALUES('x')", "INSERT INTO t VALUES(1.5)"] {
        assert_eq!(err(&mut c, bad), "datatype mismatch", "{bad}");
    }
    // A datatype mismatch is a hard error: OR IGNORE does not swallow it.
    assert_eq!(
        err(&mut c, "INSERT OR IGNORE INTO t VALUES('x')"),
        "datatype mismatch"
    );
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(0)
    );

    // Affinity-coercible values are accepted and stored as integers.
    c.execute("INSERT INTO t VALUES('5')").unwrap();
    c.execute("INSERT INTO t VALUES(2.0)").unwrap();
    c.execute("INSERT INTO t VALUES('7.0')").unwrap();
    let rows = c
        .query("SELECT a, typeof(a) FROM t ORDER BY a")
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(2), Value::Text("integer".into())],
            vec![Value::Integer(5), Value::Text("integer".into())],
            vec![Value::Integer(7), Value::Text("integer".into())],
        ]
    );
}

#[test]
fn update_integer_pk_to_non_integer_is_datatype_mismatch() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    for bad in [
        "UPDATE t SET a='x'",
        "UPDATE t SET a=1.5",
        "UPDATE t SET a=NULL", // can't null a rowid: mismatch, not NOT NULL
        "UPDATE OR IGNORE t SET a='x'",
    ] {
        assert_eq!(err(&mut c, bad), "datatype mismatch", "{bad}");
    }
    // Coercible / integer updates still work.
    c.execute("UPDATE t SET a='5'").unwrap();
    assert_eq!(
        c.query("SELECT a FROM t").unwrap().rows[0][0],
        Value::Integer(5)
    );
}

#[test]
fn upsert_do_update_integer_pk_datatype_mismatch() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(1, 1)").unwrap();
    assert_eq!(
        err(
            &mut c,
            "INSERT INTO t VALUES(1, 2) ON CONFLICT(a) DO UPDATE SET a='x'"
        ),
        "datatype mismatch"
    );
}

#[test]
fn int_primary_key_is_not_a_rowid_alias() {
    // `INT PRIMARY KEY` (not `INTEGER`) is an ordinary column: it keeps text.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO t VALUES('x')").unwrap();
    assert_eq!(
        c.query("SELECT typeof(a) FROM t").unwrap().rows[0][0],
        Value::Text("text".into())
    );
}
