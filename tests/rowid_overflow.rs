//! Rowid allocation at the `i64::MAX` boundary. graphite previously panicked
//! ("attempt to add with overflow") when the next auto-assigned rowid would
//! exceed `i64::MAX`. It now matches sqlite: an `AUTOINCREMENT` table fails with
//! `SQLITE_FULL` ("database or disk is full") — AUTOINCREMENT never reuses a
//! rowid, so there is nowhere to go — while a plain rowid / `INTEGER PRIMARY
//! KEY` table picks a random free rowid and succeeds. The resulting database
//! still passes `PRAGMA integrity_check`. Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

const MAX: i64 = i64::MAX;

#[test]
fn autoincrement_overflow_is_full_not_panic() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b)")
        .unwrap();
    c.execute(&format!("INSERT INTO t VALUES ({MAX}, 1)"))
        .unwrap();
    let err = c
        .execute("INSERT INTO t(b) VALUES (2)")
        .unwrap_err()
        .to_string();
    assert_eq!(
        err.trim_start_matches("error: "),
        "database or disk is full"
    );
    // The failed insert left the table consistent.
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

#[test]
fn plain_rowid_overflow_picks_a_free_rowid() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    c.execute(&format!("INSERT INTO t VALUES ({MAX}, 1)"))
        .unwrap();
    // No error, and the new row got some positive rowid distinct from MAX.
    c.execute("INSERT INTO t(b) VALUES (2)").unwrap();
    let r = c.query("SELECT a FROM t WHERE b = 2").unwrap();
    match r.rows[0][0] {
        Value::Integer(v) => assert!(
            v > 0 && v != MAX,
            "rowid {v} should be a free positive value"
        ),
        ref other => panic!("expected an integer rowid, got {other:?}"),
    }
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    // The AUTOINCREMENT case has a deterministic error; compare it directly.
    // (The CLI appends a " (13)" extended-result-code suffix to the library
    // message, which the bare `sqlite3_errmsg` text does not carry — strip it.)
    let sql = format!(
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b);\
         INSERT INTO t VALUES ({MAX}, 1);\
         INSERT INTO t(b) VALUES (2);"
    );
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(&sql)
        .output()
        .unwrap();
    let s_err = String::from_utf8_lossy(&out.stderr)
        .lines()
        .next()
        .unwrap_or("")
        .trim_start_matches("Error: stepping, ")
        .trim_start_matches("Error: ")
        .trim_end_matches(" (13)")
        .to_string();
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b)")
        .unwrap();
    c.execute(&format!("INSERT INTO t VALUES ({MAX}, 1)"))
        .unwrap();
    let g_err = c
        .execute("INSERT INTO t(b) VALUES (2)")
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string();
    assert_eq!(s_err, g_err);
}
