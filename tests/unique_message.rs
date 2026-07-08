//! The text of a UNIQUE-constraint violation names the offending columns, like
//! sqlite (`UNIQUE constraint failed: t.a[, t.b]`) — so code that inspects the
//! error message can tell which constraint failed. Covers the rowid / INTEGER
//! PRIMARY KEY, inline `UNIQUE`/`PRIMARY KEY`, composite, and standalone
//! `CREATE UNIQUE INDEX` cases, on both INSERT and UPDATE.

#![cfg(feature = "std")]

use graphitesql::Connection;

/// The error message from running `sql` (the second of two statements is expected
/// to fail), as a `String`.
fn violation(setup: &str, failing: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(setup).unwrap();
    c.execute(failing).unwrap_err().to_string()
}

#[test]
fn names_the_violated_columns() {
    assert!(
        violation(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, x); INSERT INTO t VALUES (1, 'a')",
            "INSERT INTO t VALUES (1, 'b')",
        )
        .contains("UNIQUE constraint failed: t.id")
    );

    assert!(
        violation(
            "CREATE TABLE t(a UNIQUE); INSERT INTO t VALUES (1)",
            "INSERT INTO t VALUES (1)",
        )
        .contains("UNIQUE constraint failed: t.a")
    );

    assert!(
        violation(
            "CREATE TABLE t(a PRIMARY KEY); INSERT INTO t VALUES (1)",
            "INSERT INTO t VALUES (1)",
        )
        .contains("UNIQUE constraint failed: t.a")
    );

    // A composite UNIQUE lists every column.
    assert!(
        violation(
            "CREATE TABLE t(a, b, UNIQUE(a, b)); INSERT INTO t VALUES (1, 2)",
            "INSERT INTO t VALUES (1, 2)",
        )
        .contains("UNIQUE constraint failed: t.a, t.b")
    );

    // A standalone CREATE UNIQUE INDEX names its column too.
    assert!(
        violation(
            "CREATE TABLE t(a, b); CREATE UNIQUE INDEX ix ON t(b); INSERT INTO t VALUES (1, 9)",
            "INSERT INTO t VALUES (2, 9)",
        )
        .contains("UNIQUE constraint failed: t.b")
    );
}

#[test]
fn names_columns_for_without_rowid() {
    // WITHOUT ROWID tables report the colliding primary-key / unique columns too.
    assert!(
        violation(
            "CREATE TABLE t(a TEXT PRIMARY KEY) WITHOUT ROWID; INSERT INTO t VALUES ('x')",
            "INSERT INTO t VALUES ('x')",
        )
        .contains("UNIQUE constraint failed: t.a")
    );
    assert!(
        violation(
            "CREATE TABLE t(a, b, PRIMARY KEY(a, b)) WITHOUT ROWID; INSERT INTO t VALUES (1, 2)",
            "INSERT INTO t VALUES (1, 2)",
        )
        .contains("UNIQUE constraint failed: t.a, t.b")
    );
}

#[test]
fn names_columns_on_update_conflict() {
    let msg = violation(
        "CREATE TABLE t(a UNIQUE); INSERT INTO t VALUES (1), (2)",
        "UPDATE t SET a = 1 WHERE a = 2",
    );
    assert!(msg.contains("UNIQUE constraint failed: t.a"), "got: {msg}");
}
