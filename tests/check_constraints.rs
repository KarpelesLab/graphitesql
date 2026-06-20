//! SQLite forbids subqueries in CHECK constraints and generated-column
//! expressions, rejecting them when the table is created. graphitesql matches
//! that (rather than silently allowing and later evaluating them).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite_ok(sql: &str) -> bool {
    Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap()
        .status
        .success()
}

fn graphite_ok(sql: &str) -> bool {
    let mut c = Connection::open_memory().unwrap();
    c.execute(sql).is_ok()
}

fn agree(sql: &str) {
    let g = graphite_ok(sql);
    if sqlite3_available() {
        assert_eq!(g, sqlite_ok(sql), "graphite/sqlite disagree on: {sql}");
    }
}

#[test]
fn subqueries_prohibited_in_check_and_generated() {
    // Rejected at CREATE.
    assert!(!graphite_ok("CREATE TABLE t(a INT CHECK(a IN (SELECT 1)))"));
    assert!(!graphite_ok("CREATE TABLE t(a INT, CHECK(a > (SELECT 1)))"));
    assert!(!graphite_ok(
        "CREATE TABLE t(a INT CHECK(EXISTS(SELECT 1)))"
    ));
    assert!(!graphite_ok("CREATE TABLE t(a INT, b AS (a + (SELECT 1)))"));

    // Ordinary CHECK / generated columns still work.
    assert!(graphite_ok("CREATE TABLE t(a INT CHECK(a > 0))"));
    assert!(graphite_ok("CREATE TABLE t(a INT, b AS (a + 1))"));

    // And they agree with the sqlite3 CLI.
    agree("CREATE TABLE t(a INT CHECK(a IN (SELECT 1)))");
    agree("CREATE TABLE t(a INT, b INT, CHECK(a > (SELECT max(b) FROM t)))");
    agree("CREATE TABLE t(a INT CHECK(a > 0))");
    agree("CREATE TABLE t(a INT, b AS (a * 2) STORED)");
}
