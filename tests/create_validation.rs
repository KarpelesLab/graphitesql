//! CREATE TABLE validations that SQLite enforces at prepare time: duplicate
//! column names, more than one PRIMARY KEY, a PRIMARY KEY/UNIQUE list naming a
//! missing column, and AUTOINCREMENT only on an INTEGER PRIMARY KEY. Each is
//! also checked against the `sqlite3` CLI's accept/reject decision.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn graphite_ok(ddl: &str) -> bool {
    Connection::open_memory().unwrap().execute(ddl).is_ok()
}

fn sqlite_ok(ddl: &str) -> bool {
    Command::new("sqlite3")
        .arg(":memory:")
        .arg(ddl)
        .output()
        .unwrap()
        .status
        .success()
}

fn agree(ddl: &str) {
    let g = graphite_ok(ddl);
    if sqlite3_available() {
        assert_eq!(g, sqlite_ok(ddl), "graphite/sqlite disagree on: {ddl}");
    }
    g.then_some(()); // silence unused in no-sqlite runs
}

#[test]
fn rejected_definitions() {
    assert!(!graphite_ok("CREATE TABLE t(a, a)"));
    assert!(!graphite_ok("CREATE TABLE t(a PRIMARY KEY, b PRIMARY KEY)"));
    assert!(!graphite_ok("CREATE TABLE t(a, b, PRIMARY KEY(a, z))"));
    assert!(!graphite_ok("CREATE TABLE t(a, UNIQUE(zzz))"));
    assert!(!graphite_ok(
        "CREATE TABLE t(a TEXT PRIMARY KEY AUTOINCREMENT)"
    ));
    assert!(!graphite_ok("CREATE TABLE t(a PRIMARY KEY AUTOINCREMENT)"));
    assert!(!graphite_ok(
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT) WITHOUT ROWID"
    ));
    assert!(!graphite_ok(
        "CREATE TABLE t(x INT GENERATED ALWAYS AS (1) VIRTUAL)"
    ));
}

#[test]
fn accepted_definitions() {
    assert!(graphite_ok("CREATE TABLE t(a, b)"));
    assert!(graphite_ok("CREATE TABLE t(a PRIMARY KEY, b)"));
    assert!(graphite_ok("CREATE TABLE t(a, b, PRIMARY KEY(a, b))"));
    assert!(graphite_ok(
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b)"
    ));
    assert!(graphite_ok("CREATE TABLE t(a, b AS (a + 1))"));
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    for ddl in [
        "CREATE TABLE t(a, a)",
        "CREATE TABLE t(a PRIMARY KEY, b PRIMARY KEY)",
        "CREATE TABLE t(a, b, PRIMARY KEY(a, z))",
        "CREATE TABLE t(a TEXT PRIMARY KEY AUTOINCREMENT)",
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b)",
        "CREATE TABLE t(x INT GENERATED ALWAYS AS (1) VIRTUAL)",
        "CREATE TABLE t(a, b, PRIMARY KEY(a, b))",
    ] {
        agree(ddl);
    }
}
