//! STRICT tables (`CREATE TABLE … STRICT`): rigid per-column typing. Every cell
//! of the type/value matrix is matched against the real `sqlite3` CLI — both the
//! accept/reject decision and the resulting `typeof`/`quote` of stored values —
//! and a written STRICT database is gated on `PRAGMA integrity_check` and read
//! back by `sqlite3` (which must agree it is a strict table).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// `(stored, typeof, quote)` for the single row, or `Err(message)`.
fn graphite_insert(col: &str, val: &str) -> Result<String, ()> {
    let mut c = Connection::open_memory().unwrap();
    if c.execute(&format!("CREATE TABLE t(a {col}) STRICT"))
        .is_err()
    {
        return Err(());
    }
    if c.execute(&format!("INSERT INTO t VALUES({val})")).is_err() {
        return Err(());
    }
    let r = c
        .query("SELECT typeof(a) || '=' || quote(a) FROM t")
        .unwrap();
    match &r.rows[0][0] {
        Value::Text(t) => Ok(String::from(t.as_str())),
        _ => Ok(String::new()),
    }
}

fn sqlite_insert(col: &str, val: &str) -> Result<String, ()> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!(
            "CREATE TABLE t(a {col}) STRICT; INSERT INTO t VALUES({val}); \
             SELECT typeof(a) || '=' || quote(a) FROM t;"
        ))
        .output()
        .unwrap();
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(())
    }
}

#[test]
fn strict_type_matrix_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let cols = ["INT", "INTEGER", "REAL", "TEXT", "BLOB", "ANY"];
    let vals = [
        "5", "5.0", "5.5", "'5'", "'5.5'", "'hello'", "NULL", "x'41'",
    ];
    for col in cols {
        for val in vals {
            let g = graphite_insert(col, val);
            let s = sqlite_insert(col, val);
            match (&g, &s) {
                (Ok(gv), Ok(sv)) => {
                    assert_eq!(gv, sv, "stored value diverged for {col} <- {val}")
                }
                (Err(_), Err(_)) => {} // both rejected
                _ => panic!("accept/reject diverged for {col} <- {val}: g={g:?} s={s:?}"),
            }
        }
    }
}

#[test]
fn strict_rejects_invalid_column_types() {
    // Only INT/INTEGER/REAL/TEXT/BLOB/ANY are allowed in a STRICT table.
    for bad in ["VARCHAR", "NUMERIC", "FLOAT", "BOOLEAN"] {
        let mut c = Connection::open_memory().unwrap();
        assert!(
            c.execute(&format!("CREATE TABLE t(a {bad}) STRICT"))
                .is_err(),
            "STRICT should reject column type {bad}"
        );
    }
    // A column with no declared type is also rejected.
    let mut c = Connection::open_memory().unwrap();
    assert!(c.execute("CREATE TABLE t(a) STRICT").is_err());
    // The six allowed types are accepted.
    for ok in ["INT", "INTEGER", "REAL", "TEXT", "BLOB", "ANY"] {
        let mut c = Connection::open_memory().unwrap();
        assert!(
            c.execute(&format!("CREATE TABLE t(a {ok}) STRICT")).is_ok(),
            "STRICT should accept column type {ok}"
        );
    }
}

#[test]
fn strict_enforced_on_update_and_combined_without_rowid() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER, b TEXT) STRICT")
        .unwrap();
    c.execute("INSERT INTO t VALUES(1, 'x')").unwrap();
    // UPDATE that would store a non-integer in an INTEGER column is rejected.
    assert!(c.execute("UPDATE t SET a = 'oops' WHERE b = 'x'").is_err());
    assert!(c.execute("UPDATE t SET a = 2 WHERE b = 'x'").is_ok());

    // STRICT combines with WITHOUT ROWID in either order.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(k INTEGER PRIMARY KEY, v TEXT) STRICT, WITHOUT ROWID")
        .unwrap();
    c.execute("INSERT INTO u VALUES(1, 'a')").unwrap();
    assert!(c.execute("INSERT INTO u VALUES('x', 'b')").is_err());
    assert!(c.execute("UPDATE u SET v = 9 WHERE k = 1").is_ok()); // 9 -> text '9'
    let mut c = Connection::open_memory().unwrap();
    assert!(
        c.execute("CREATE TABLE w(k INTEGER PRIMARY KEY) WITHOUT ROWID, STRICT")
            .is_ok()
    );
}

#[test]
fn strict_file_roundtrips_and_sqlite_agrees() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-strict-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(a INTEGER, b TEXT, c ANY) STRICT")
            .unwrap();
        c.execute("INSERT INTO t VALUES(1, 'x', 3.5)").unwrap();
    }
    // Reopen: enforcement survives a re-parse of the stored schema SQL.
    {
        let mut c = Connection::open(&path).unwrap();
        assert!(c.execute("INSERT INTO t VALUES('hello', 'y', 1)").is_err());
        assert!(c.execute("INSERT INTO t VALUES(2, 'z', x'00')").is_ok());
    }
    // sqlite3 validates the file and enforces STRICT on it too.
    let check = Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check;")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "ok");
    let reject = Command::new("sqlite3")
        .arg(&path)
        .arg("INSERT INTO t VALUES('nope', 'q', 1);")
        .output()
        .unwrap();
    assert!(
        !reject.status.success(),
        "sqlite3 should reject a TEXT value in the STRICT INTEGER column"
    );
    let _ = std::fs::remove_file(&path);
}
