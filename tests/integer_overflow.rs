//! Integer-overflow semantics that distinguish `sum()`/`abs()` from the
//! arithmetic operators. SQLite raises "integer overflow" when an integer
//! `sum()` accumulator or `abs()` cannot be represented in i64, but the `+`/`*`
//! operators fall back to real instead. Matched against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Whether `sqlite3` evaluates `sql` without an error.
fn sqlite_ok(sql: &str) -> bool {
    Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap()
        .status
        .success()
}

/// Build an in-memory connection from setup statements.
fn conn(setup: &[&str]) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for s in setup {
        c.execute(s).unwrap();
    }
    c
}

#[test]
fn sum_integer_overflow_errors() {
    // sum() over integers overflowing i64 is an error.
    let c = conn(&[
        "CREATE TABLE t(x INTEGER)",
        "INSERT INTO t VALUES(9223372036854775807),(1)",
    ]);
    assert!(c.query("SELECT sum(x) FROM t").is_err());

    // A non-overflowing integer sum is fine and stays integer.
    let c = conn(&["CREATE TABLE t(x INTEGER)", "INSERT INTO t VALUES(2),(3)"]);
    assert_eq!(
        c.query("SELECT sum(x) FROM t").unwrap().rows[0][0],
        Value::Integer(5)
    );

    // With a real present, sum accumulates as real and never overflows.
    let c = conn(&[
        "CREATE TABLE t(x)",
        "INSERT INTO t VALUES(9223372036854775807),(1.0)",
    ]);
    assert!(matches!(
        c.query("SELECT sum(x) FROM t").unwrap().rows[0][0],
        Value::Real(_)
    ));

    // total() never errors — it is always a real sum.
    let c = conn(&[
        "CREATE TABLE t(x INTEGER)",
        "INSERT INTO t VALUES(9223372036854775807),(1)",
    ]);
    assert!(matches!(
        c.query("SELECT total(x) FROM t").unwrap().rows[0][0],
        Value::Real(_)
    ));
}

#[test]
fn abs_of_min_integer_errors() {
    let c = conn(&[
        "CREATE TABLE t(x INTEGER)",
        "INSERT INTO t VALUES(-9223372036854775807 - 1)", // i64::MIN
    ]);
    assert_eq!(
        c.query("SELECT typeof(x) FROM t").unwrap().rows[0][0],
        Value::Text("integer".into())
    );
    assert!(c.query("SELECT abs(x) FROM t").is_err());
    // abs of a representable integer is fine.
    assert_eq!(
        c.query("SELECT abs(-5)").unwrap().rows[0][0],
        Value::Integer(5)
    );
}

#[test]
fn min_int_literal_folds_to_integer() {
    let c = Connection::open_memory().unwrap();
    // `-9223372036854775808` is exactly i64::MIN: an integer, not a real.
    assert_eq!(
        c.query("SELECT -9223372036854775808").unwrap().rows[0][0],
        Value::Integer(i64::MIN)
    );
    assert_eq!(
        c.query("SELECT typeof(-9223372036854775808)").unwrap().rows[0][0],
        Value::Text("integer".into())
    );
    // A leading whitespace between the sign and digits still folds.
    assert_eq!(
        c.query("SELECT - 9223372036854775808").unwrap().rows[0][0],
        Value::Integer(i64::MIN)
    );
    // Used positively, the 2^63 magnitude overflows i64 and is a real.
    assert!(matches!(
        c.query("SELECT 9223372036854775808").unwrap().rows[0][0],
        Value::Real(_)
    ));
    // abs() of the folded i64::MIN therefore overflows, like SQLite.
    assert!(c.query("SELECT abs(-9223372036854775808)").is_err());
}

#[test]
fn operators_fall_back_to_real_not_error() {
    // `+`/`*` overflow promotes to real (no error), unlike sum()/abs().
    let c = Connection::open_memory().unwrap();
    assert!(matches!(
        c.query("SELECT 9223372036854775807 + 1").unwrap().rows[0][0],
        Value::Real(_)
    ));
    assert!(matches!(
        c.query("SELECT 9223372036854775807 * 2").unwrap().rows[0][0],
        Value::Real(_)
    ));
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    // (graphite query, sqlite sql) — both should agree on error vs. success.
    let cases: &[&str] = &[
        "SELECT sum(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1)",
        "SELECT abs(-9223372036854775807 - 1)",
        "SELECT 9223372036854775807 + 1",
        "SELECT abs(-5)",
        "SELECT total(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1)",
    ];
    for sql in cases {
        let c = Connection::open_memory().unwrap();
        let g = c.query(sql).is_ok();
        let s = sqlite_ok(sql);
        assert_eq!(g, s, "accept/reject diverged on {sql}");
    }
}
