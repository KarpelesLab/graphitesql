//! Differential tests for [`graphitesql::Session`] changeset generation.
//!
//! graphite's changeset bytes are compared, byte-for-byte, against the SQLite
//! session extension. SQLite's `sqlite3` CLI does not expose sessions, so the
//! oracle is a tiny C harness (`sesdump`) built against the amalgamation with
//! `SQLITE_ENABLE_SESSION`. Point `GRAPHITE_SESDUMP` at that binary to run the
//! differential half; the byte-literal assertions always run.
//!
//! `sesdump` usage: `sesdump <db> <sql> [setup-sql]` — it creates a session on
//! `main`, attaches all tables, runs `<sql>`, and prints the changeset as hex.
//! `[setup-sql]` (schema + seed rows) runs *before* the session is created, so
//! those rows are not themselves recorded.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

/// Run `sql` (optionally after `setup`) on a fresh in-memory graphite
/// connection with a session attached, and return the changeset as lowercase
/// hex.
fn graphite_changeset(setup: &str, sql: &str) -> String {
    let mut conn = Connection::open_memory().unwrap();
    if !setup.is_empty() {
        conn.execute_batch(setup).unwrap();
    }
    let session = conn.create_session();
    session.attach();
    conn.execute_batch(sql).unwrap();
    let bytes = conn.session_changeset(&session).unwrap();
    hex(&bytes)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Ask the SQLite oracle for the reference changeset hex, or `None` if the
/// oracle binary is not configured.
fn oracle(setup: &str, sql: &str) -> Option<String> {
    let bin = std::env::var("GRAPHITE_SESDUMP").ok()?;
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(sql)
        .arg(setup)
        .output()
        .expect("run sesdump");
    assert!(
        out.status.success(),
        "sesdump failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Assert graphite's changeset equals the oracle's (when configured) and, when
/// `expect` is `Some`, equals that byte literal too.
fn check(setup: &str, sql: &str, expect: Option<&str>) {
    let got = graphite_changeset(setup, sql);
    if let Some(reference) = oracle(setup, sql) {
        assert_eq!(got, reference, "vs oracle\n setup={setup}\n sql={sql}");
    }
    if let Some(exp) = expect {
        assert_eq!(got, exp, "vs literal\n setup={setup}\n sql={sql}");
    }
}

const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b);";

#[test]
fn insert_int() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        Some("5402010074001200010000000000000001010000000000000002"),
    );
}

#[test]
fn insert_text() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,'hi');",
        Some("540201007400120001000000000000000103026869"),
    );
}

#[test]
fn insert_real() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,3.5);",
        Some("540201007400120001000000000000000102400c000000000000"),
    );
}

#[test]
fn insert_null() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,NULL);",
        Some("540201007400120001000000000000000105"),
    );
}

#[test]
fn insert_blob() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,x'aabb');",
        Some("54020100740012000100000000000000010402aabb"),
    );
}

#[test]
fn insert_two_rows() {
    check(
        "",
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2),(3,4);",
        Some(
            "540201007400120001000000000000000101000000000000000212000100000000\
             00000003010000000000000004",
        ),
    );
}

#[test]
fn delete_row() {
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "DELETE FROM t WHERE a=1;",
        Some("5402010074000900010000000000000001010000000000000002"),
    );
}

#[test]
fn update_int() {
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "UPDATE t SET b=99 WHERE a=1;",
        Some("540201007400170001000000000000000101000000000000000200010000000000000063"),
    );
}

#[test]
fn update_text() {
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "UPDATE t SET b='xyz' WHERE a=1;",
        Some("540201007400170001000000000000000101000000000000000200030378797a"),
    );
}

// The following exercise coalescing and multi-row hash ordering; the exact
// bytes are validated against the oracle (when configured). Literals are
// omitted for the ordering cases — the oracle is authoritative there.

#[test]
fn insert_then_update_coalesces_to_insert() {
    check(
        SCHEMA,
        "INSERT INTO t VALUES(1,2); UPDATE t SET b=99 WHERE a=1;",
        // final INSERT of (1, 99)
        Some("5402010074001200010000000000000001010000000000000063"),
    );
}

#[test]
fn insert_then_delete_coalesces_to_nothing() {
    check(
        SCHEMA,
        "INSERT INTO t VALUES(1,2); DELETE FROM t WHERE a=1;",
        Some(""),
    );
}

#[test]
fn update_then_delete_coalesces_to_delete_of_original() {
    // (1,2) is seeded outside the session; the session then updates and deletes
    // it, which must coalesce to a DELETE carrying the *original* values.
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "UPDATE t SET b=5 WHERE a=1; DELETE FROM t WHERE a=1;",
        Some("5402010074000900010000000000000001010000000000000002"),
    );
}

#[test]
fn multi_row_hash_order() {
    // Ten rows inserted out of order — the changeset lists them in SQLite's
    // hash-bucket order, not insertion or rowid order. Oracle-checked.
    check(
        SCHEMA,
        "INSERT INTO t VALUES(5,0),(1,0),(9,0),(3,0),(7,0),(2,0),(8,0),(4,0),(6,0),(10,0);",
        None,
    );
}

#[test]
fn delete_then_insert_coalesces_to_update() {
    // A row seeded outside the session, then deleted and re-inserted with a new
    // value inside it, coalesces to an UPDATE (old = pre-delete, new = final) —
    // matching SQLite, which decides the emitted op from the live row.
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "DELETE FROM t WHERE a=1; INSERT INTO t VALUES(1,9);",
        Some("540201007400170001000000000000000101000000000000000200010000000000000009"),
    );
}

#[test]
fn insert_or_replace_same_pk_is_update() {
    // `INSERT OR REPLACE` over an existing row (same PK, seeded outside the
    // session) is recorded as an UPDATE, like SQLite.
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "INSERT OR REPLACE INTO t VALUES(1,9);",
        Some("540201007400170001000000000000000101000000000000000200010000000000000009"),
    );
}

#[test]
fn is_empty_when_no_changes() {
    let conn = Connection::open_memory().unwrap();
    let session = conn.create_session();
    session.attach();
    assert!(session.is_empty());
    assert_eq!(conn.session_changeset(&session).unwrap(), Vec::<u8>::new());
}

#[test]
fn no_op_update_produces_nothing() {
    // Updating a column to its current value is a no-op change: SQLite emits
    // nothing for it.
    check(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b); INSERT INTO t VALUES(1,2);",
        "UPDATE t SET b=2 WHERE a=1;",
        Some(""),
    );
}
