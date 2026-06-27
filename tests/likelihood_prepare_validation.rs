//! `likelihood(X, prob)` is validated by SQLite during analysis, not per row:
//! the call must have exactly two arguments and the probability must be a
//! floating-point literal in `0.0..=1.0` (`exprProbability` in `expr.c`). Both
//! errors therefore fire even when the query produces no rows — over an empty or
//! fully-filtered table — where graphite previously deferred the check to the
//! evaluator and so accepted the bad call. A valid probability still returns the
//! operand unchanged, and the check covers every scalar-expression position
//! (result list, WHERE, GROUP BY, ORDER BY, join ON, nested function argument).
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The library's error message for `sql` run on `c`, with the Display tag
/// stripped. Runs on a caller-supplied connection so a `FROM` table set up
/// beforehand is in scope (otherwise `no such table` would mask the real error).
fn err_on(c: &Connection, sql: &str) -> String {
    let e = c.query(sql).unwrap_err().to_string();
    e.trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .to_string()
}

/// The single scalar value of a one-row, one-column query.
fn scalar(sql: &str) -> Value {
    let c = Connection::open_memory().unwrap();
    c.query(sql).unwrap().rows.into_iter().next().unwrap()[0].clone()
}

const RANGE: &str = "second argument to likelihood() must be a constant between 0.0 and 1.0";
const ARITY: &str = "wrong number of arguments to function likelihood()";

#[test]
fn out_of_range_probability_is_caught_over_an_empty_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    // No rows, yet the bad probability literal is rejected at prepare time.
    assert_eq!(err_on(&c, "SELECT likelihood(a, 2) FROM t"), RANGE);
    assert_eq!(err_on(&c, "SELECT likelihood(a, -0.1) FROM t"), RANGE);
    assert_eq!(err_on(&c, "SELECT likelihood(a, 1.5) FROM t"), RANGE);
    // An integer literal in range is still not a float literal → rejected.
    assert_eq!(err_on(&c, "SELECT likelihood(a, 1) FROM t"), RANGE);
    // A fully-filtered table is likewise empty at run time.
    assert_eq!(
        err_on(&c, "SELECT likelihood(a, 2) FROM t WHERE a > 5"),
        RANGE
    );
}

#[test]
fn wrong_arity_is_caught_over_an_empty_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    assert_eq!(err_on(&c, "SELECT likelihood(a) FROM t"), ARITY);
    assert_eq!(err_on(&c, "SELECT likelihood(a, 0.5, 0.6) FROM t"), ARITY);
}

#[test]
fn the_check_covers_every_clause_position() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    assert_eq!(err_on(&c, "SELECT 1 FROM t WHERE likelihood(a, 2)"), RANGE);
    assert_eq!(
        err_on(&c, "SELECT 1 FROM t GROUP BY likelihood(a, 2)"),
        RANGE
    );
    assert_eq!(
        err_on(&c, "SELECT 1 FROM t ORDER BY likelihood(a, 2)"),
        RANGE
    );
    assert_eq!(
        err_on(&c, "SELECT 1 FROM t JOIN t t2 ON likelihood(t.a, 2)"),
        RANGE
    );
    // Nested inside another scalar function's argument.
    assert_eq!(err_on(&c, "SELECT abs(likelihood(a, 2)) FROM t"), RANGE);
}

#[test]
fn a_valid_probability_returns_the_operand() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    // Empty table: a valid call prepares cleanly and yields no rows (no error).
    assert!(c
        .query("SELECT likelihood(a, 0.5) FROM t")
        .unwrap()
        .rows
        .is_empty());
    // With a row, the operand passes through unchanged.
    assert_eq!(scalar("SELECT likelihood(42, 0.5)"), Value::Integer(42));
    assert_eq!(scalar("SELECT likelihood(7, 1.0)"), Value::Integer(7));
    assert_eq!(scalar("SELECT likelihood(7, 0.0)"), Value::Integer(7));
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout.lines().next().unwrap_or("").trim_end().to_string();
        if !line.is_empty() {
            return line;
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "CREATE TABLE t(a); SELECT likelihood(a, 2) FROM t",
        "CREATE TABLE t(a); SELECT likelihood(a, -0.1) FROM t",
        "CREATE TABLE t(a); SELECT likelihood(a) FROM t",
        "CREATE TABLE t(a); SELECT likelihood(a, 0.5, 0.6) FROM t",
        "CREATE TABLE t(a); SELECT 1 FROM t WHERE likelihood(a, 2)",
        "CREATE TABLE t(a); SELECT 1 FROM t GROUP BY likelihood(a, 2)",
        "CREATE TABLE t(a); SELECT 1 FROM t ORDER BY likelihood(a, 2)",
        "CREATE TABLE t(a); SELECT abs(likelihood(a, 2)) FROM t",
        "CREATE TABLE t(a); SELECT likelihood(a, 0.5) FROM t",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT likelihood(a, 2) FROM t",
        "SELECT likelihood(42, 0.5)",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
