//! A `DISTINCT` aggregate must have exactly one argument. SQLite reports this as
//! `DISTINCT aggregates must have exactly one argument`, but only once the call
//! is within the function's normal upper arity bound — so `count(DISTINCT 1,2)`
//! and `sum(DISTINCT 1,2)` (which exceed their 1-argument limit) keep reporting
//! the plain `wrong number of arguments` error, while `count(DISTINCT)` (0 args,
//! valid as `count(*)`) and `group_concat(DISTINCT a,b)` (within its 2-argument
//! limit) report the DISTINCT message. Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn graphite_err(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

fn sqlite_err(sql: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .next()
        .unwrap_or("")
        .trim_start_matches("Error: in prepare, ")
        .trim_start_matches("Error: stepping, ")
        .trim_start_matches("Error: ")
        .to_string()
}

#[test]
fn distinct_aggregate_arity_message() {
    let c = Connection::open_memory().unwrap();
    const DISTINCT_MSG: &str = "DISTINCT aggregates must have exactly one argument";

    // The DISTINCT-arity message: zero or (within-bound) multi-argument DISTINCT.
    for sql in [
        "SELECT count(DISTINCT)",
        "SELECT group_concat(DISTINCT 1,2)",
        "SELECT string_agg(DISTINCT 1,2)",
        "SELECT json_group_object(DISTINCT 'a',1)",
    ] {
        assert_eq!(graphite_err(&c, sql), DISTINCT_MSG, "for {sql}");
    }

    // Exceeding the function's upper arity bound keeps the plain arity error,
    // even with DISTINCT, because that bound is checked first.
    for (sql, name) in [
        ("SELECT count(DISTINCT 1,2)", "count"),
        ("SELECT sum(DISTINCT 1,2)", "sum"),
        ("SELECT avg(DISTINCT 1,2)", "avg"),
        ("SELECT total(DISTINCT 1,2)", "total"),
    ] {
        assert_eq!(
            graphite_err(&c, sql),
            format!("wrong number of arguments to function {name}()"),
            "for {sql}"
        );
    }

    // A single-argument DISTINCT aggregate is accepted (returns a value).
    for sql in [
        "SELECT count(DISTINCT 1)",
        "SELECT sum(DISTINCT 1)",
        "SELECT group_concat(DISTINCT 1)",
    ] {
        assert!(c.query(sql).is_ok(), "{sql} should be accepted");
    }
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT count(DISTINCT)",
        "SELECT count(DISTINCT 1,2)",
        "SELECT sum(DISTINCT 1,2)",
        "SELECT avg(DISTINCT 1,2)",
        "SELECT total(DISTINCT 1,2)",
        "SELECT group_concat(DISTINCT 1,2)",
        "SELECT string_agg(DISTINCT 1,2)",
        "SELECT json_group_object(DISTINCT 'a',1)",
    ] {
        assert_eq!(graphite_err(&c, sql), sqlite_err(sql), "for {sql}");
    }
}
