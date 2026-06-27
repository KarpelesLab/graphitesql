//! SQLite forbids nesting an aggregate (or a window function) inside another
//! aggregate's argument: the argument of an aggregate is resolved with
//! `NC_InAggFunc` set, so `sum(count(*))` is a `misuse of aggregate function
//! count()` and `sum(row_number() OVER ())` is a `misuse of window function
//! row_number()`. SQLite rejects both during analysis, so the error fires even
//! when the query produces no rows — over an empty or fully-filtered table —
//! where graphite's lazy evaluator previously accepted the nesting (and even
//! mis-evaluated it: `count(sum(a))` returned `0` instead of erroring). A scalar
//! wrapper (`abs(count(*))`, `max(sum(a), 1)`) is fine, an aggregate used as a
//! window (`sum(a) OVER ()`) is fine, and a nested aggregate in a *subquery* is a
//! separate query level and fine. Verified against the sqlite3 3.50.4 CLI.

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

#[test]
fn a_nested_aggregate_is_caught_over_an_empty_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    assert_eq!(
        err_on(&c, "SELECT sum(count(*)) FROM t"),
        "misuse of aggregate function count()"
    );
    assert_eq!(
        err_on(&c, "SELECT count(sum(a)) FROM t"),
        "misuse of aggregate function sum()"
    );
    assert_eq!(
        err_on(&c, "SELECT sum(sum(a)) FROM t"),
        "misuse of aggregate function sum()"
    );
    // The nested aggregate may be buried under a scalar wrapper or arithmetic.
    assert_eq!(
        err_on(&c, "SELECT abs(sum(count(*))) FROM t"),
        "misuse of aggregate function count()"
    );
    assert_eq!(
        err_on(&c, "SELECT sum(a + count(*)) FROM t"),
        "misuse of aggregate function count()"
    );
}

#[test]
fn a_nested_window_is_caught_over_an_empty_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    assert_eq!(
        err_on(&c, "SELECT sum(row_number() OVER ()) FROM t"),
        "misuse of window function row_number()"
    );
    assert_eq!(
        err_on(&c, "SELECT sum(a + row_number() OVER ()) FROM t"),
        "misuse of window function row_number()"
    );
}

#[test]
fn the_innermost_call_in_source_order_is_named() {
    // When an aggregate's argument holds both a nested aggregate and a nested
    // window, SQLite names whichever it resolves last in source order.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    assert_eq!(
        err_on(&c, "SELECT sum(count(*) + row_number() OVER ()) FROM t"),
        "misuse of window function row_number()"
    );
    assert_eq!(
        err_on(&c, "SELECT sum(row_number() OVER () + count(*)) FROM t"),
        "misuse of aggregate function count()"
    );
}

#[test]
fn the_check_covers_having_and_order_by() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    assert_eq!(
        err_on(&c, "SELECT a FROM t GROUP BY a HAVING sum(count(*))"),
        "misuse of aggregate function count()"
    );
    assert_eq!(
        err_on(&c, "SELECT sum(a) FROM t ORDER BY sum(count(*))"),
        "misuse of aggregate function count()"
    );
    // A window nested in an ORDER BY aggregate is named ahead of the outer
    // aggregate's own (otherwise-applicable) misuse.
    assert_eq!(
        err_on(&c, "SELECT a FROM t ORDER BY sum(row_number() OVER ())"),
        "misuse of window function row_number()"
    );
}

#[test]
fn legitimate_nesting_is_still_accepted() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3)").unwrap();
    // A scalar wrapper around an aggregate is fine.
    assert_eq!(
        scalar("SELECT abs(sum(a)) FROM (SELECT 1 a)"),
        Value::Integer(1)
    );
    // The two-arg (scalar) max/min may nest an aggregate.
    assert_eq!(
        c.query("SELECT max(sum(a), 1) FROM t")
            .unwrap()
            .rows
            .into_iter()
            .next()
            .unwrap()[0],
        Value::Integer(6)
    );
    // An aggregate used as a window is not a nesting site.
    assert_eq!(
        c.query("SELECT sum(a) OVER () FROM t LIMIT 1")
            .unwrap()
            .rows
            .into_iter()
            .next()
            .unwrap()[0],
        Value::Integer(6)
    );
    // A nested aggregate inside a subquery is a separate query level.
    assert_eq!(
        c.query("SELECT sum((SELECT count(*) FROM t)) FROM t")
            .unwrap()
            .rows
            .into_iter()
            .next()
            .unwrap()[0],
        Value::Integer(9)
    );
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
        "CREATE TABLE t(a); SELECT sum(count(*)) FROM t",
        "CREATE TABLE t(a); SELECT count(sum(a)) FROM t",
        "CREATE TABLE t(a); SELECT abs(sum(count(*))) FROM t",
        "CREATE TABLE t(a); SELECT total(avg(a)) FROM t",
        "CREATE TABLE t(a); SELECT sum(row_number() OVER ()) FROM t",
        "CREATE TABLE t(a); SELECT sum(a + row_number() OVER ()) FROM t",
        "CREATE TABLE t(a); SELECT sum(count(*) + row_number() OVER ()) FROM t",
        "CREATE TABLE t(a); SELECT sum(row_number() OVER () + count(*)) FROM t",
        "CREATE TABLE t(a); SELECT a FROM t GROUP BY a HAVING sum(count(*))",
        "CREATE TABLE t(a); SELECT sum(a) FROM t ORDER BY sum(count(*))",
        "CREATE TABLE t(a); SELECT a FROM t ORDER BY sum(row_number() OVER ())",
        // With rows present, the same misuse is reported (not silently evaluated).
        "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2); SELECT count(sum(a)) FROM t",
        // Legitimate forms still compute.
        "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2); SELECT max(sum(a), 1) FROM t",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2); SELECT sum(abs(a)) FROM t",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
