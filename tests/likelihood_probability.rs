//! `likelihood(X, Y)` is an optimizer hint that returns `X` unchanged, but its
//! second argument must be a floating-point *literal* in `0.0..=1.0`. SQLite
//! checks this against the parsed AST (`exprProbability` in `expr.c`), so an
//! integer literal, a negative, a string, a column reference, or any compound
//! expression is rejected with `second argument to likelihood() must be a
//! constant between 0.0 and 1.0` — while `0.5`, `.5`, `1e0` and a parenthesized
//! `(0.5)` are accepted. Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// graphite's accept/reject for a one-shot statement.
fn graphite_ok(sql: &str) -> bool {
    Connection::open_memory().unwrap().query(sql).is_ok()
}

/// sqlite3 CLI's accept/reject for the same statement.
fn sqlite_ok(sql: &str) -> bool {
    Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn second_argument_must_be_a_float_literal_in_range() {
    let c = Connection::open_memory().unwrap();
    // Accepted: a bare float literal in [0.0, 1.0], including exponent and
    // leading-dot forms and a redundant parenthesization.
    for sql in [
        "SELECT likelihood(5, 0.5)",
        "SELECT likelihood(5, 0.0)",
        "SELECT likelihood(5, 1.0)",
        "SELECT likelihood(5, .5)",
        "SELECT likelihood(5, 0.5e0)",
        "SELECT likelihood(5, 1e0)",
        "SELECT likelihood(5, (0.5))",
    ] {
        assert_eq!(
            c.query(sql).unwrap().rows.remove(0).remove(0),
            Value::Integer(5),
            "{sql} should return its first argument"
        );
    }

    // Rejected: integer literals (even 0 and 1), out-of-range, negative, string,
    // and any compound expression — none is a bare in-range float literal.
    for sql in [
        "SELECT likelihood(5, 0)",
        "SELECT likelihood(5, 1)",
        "SELECT likelihood(5, 2)",
        "SELECT likelihood(5, 1.5)",
        "SELECT likelihood(5, -0.1)",
        "SELECT likelihood(5, 'x')",
        "SELECT likelihood(5, 0.5+0.1)",
        "SELECT likelihood(5, abs(0.5))",
    ] {
        let err = c
            .query(sql)
            .unwrap_err()
            .to_string()
            .trim_start_matches("error: ")
            .to_string();
        assert_eq!(
            err, "second argument to likelihood() must be a constant between 0.0 and 1.0",
            "unexpected message for {sql}"
        );
    }
}

#[test]
fn likelihood_passes_a_column_through_unchanged() {
    // With a non-constant first argument the call still returns it row-by-row,
    // and a valid literal probability is accepted.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(10),(20),(30)").unwrap();
    let rows = c
        .query("SELECT likelihood(a, 0.25) FROM t ORDER BY a")
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(10)],
            vec![Value::Integer(20)],
            vec![Value::Integer(30)],
        ]
    );
    // A bad probability with a column first argument is still rejected.
    assert!(c.query("SELECT likelihood(a, 2) FROM t").is_err());
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    for sql in [
        "SELECT likelihood(5, 0.5)",
        "SELECT likelihood(5, 0.0)",
        "SELECT likelihood(5, 1.0)",
        "SELECT likelihood(5, .5)",
        "SELECT likelihood(5, 0.5e0)",
        "SELECT likelihood(5, (0.5))",
        "SELECT likelihood(5, 0)",
        "SELECT likelihood(5, 1)",
        "SELECT likelihood(5, 2)",
        "SELECT likelihood(5, 1.5)",
        "SELECT likelihood(5, -0.1)",
        "SELECT likelihood(5, 'x')",
        "SELECT likelihood(5, 0.5+0.1)",
    ] {
        assert_eq!(
            graphite_ok(sql),
            sqlite_ok(sql),
            "graphite/sqlite disagree on: {sql}"
        );
    }
}
