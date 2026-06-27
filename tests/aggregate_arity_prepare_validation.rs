//! SQLite validates the *arity* of an aggregate function during analysis,
//! before any row is produced and ahead of the placement/misuse checks. So
//! `sum()`, `sum(a, a)`, `count(1, 2)` and friends report `wrong number of
//! arguments to function NAME()` even in clauses where the aggregate would
//! otherwise be a misuse (`WHERE sum()`, `ORDER BY sum(a,a)`) and even over an
//! empty or fully-filtered table where graphite's lazy evaluator previously
//! reached neither check. The bound counts mirror SQLite's: `count` takes 0 or
//! 1, `group_concat`/`string_agg`/`json_group_object` take up to 2, the rest
//! take exactly 1. Multi-arg `min`/`max` stay scalar (not validated here).
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

#[test]
fn too_few_arguments_is_caught_over_an_empty_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    for sql in [
        "SELECT sum() FROM t GROUP BY a",
        "SELECT a FROM t GROUP BY a HAVING avg()",
        "SELECT total() FROM t",
        "SELECT group_concat() FROM t",
    ] {
        let want = alloc_name(sql);
        assert_eq!(err_on(&c, sql), want, "for {sql}");
    }
}

#[test]
fn too_many_arguments_is_caught_over_an_empty_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    for sql in [
        "SELECT sum(a, a) FROM t",
        "SELECT avg(a, a) FROM t GROUP BY a",
        "SELECT count(1, 2) FROM t",
        "SELECT total(a, a) FROM t",
    ] {
        let want = alloc_name(sql);
        assert_eq!(err_on(&c, sql), want, "for {sql}");
    }
}

#[test]
fn arity_is_checked_in_every_clause() {
    // The arity error fires ahead of the placement/misuse check in clauses
    // where the aggregate is itself misplaced (WHERE / ORDER BY / GROUP BY).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    assert_eq!(
        err_on(&c, "SELECT a FROM t WHERE sum(a, a)"),
        "wrong number of arguments to function sum()"
    );
    assert_eq!(
        err_on(&c, "SELECT a FROM t ORDER BY sum(a, a)"),
        "wrong number of arguments to function sum()"
    );
    assert_eq!(
        err_on(&c, "SELECT a FROM t GROUP BY avg(a, a)"),
        "wrong number of arguments to function avg()"
    );
    assert_eq!(
        err_on(&c, "SELECT a FROM t GROUP BY a HAVING sum(a, a)"),
        "wrong number of arguments to function sum()"
    );
    // But a HAVING in a non-aggregate query (no GROUP BY, no result aggregate)
    // is itself rejected ahead of the arity check, matching SQLite.
    assert_eq!(
        err_on(&c, "SELECT a FROM t HAVING sum(a, a)"),
        "HAVING clause on a non-aggregate query"
    );
}

#[test]
fn legitimate_arities_are_still_accepted() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1, 10), (2, 20)").unwrap();
    let one =
        |sql: &str| -> Value { c.query(sql).unwrap().rows.into_iter().next().unwrap()[0].clone() };
    assert_eq!(one("SELECT sum(a) FROM t"), Value::Integer(3));
    assert_eq!(one("SELECT count() FROM t"), Value::Integer(2));
    assert_eq!(one("SELECT count(*) FROM t"), Value::Integer(2));
    assert_eq!(one("SELECT count(a) FROM t"), Value::Integer(2));
    assert_eq!(
        one("SELECT group_concat(a) FROM t"),
        Value::Text("1,2".into())
    );
    assert_eq!(
        one("SELECT group_concat(a, '-') FROM t"),
        Value::Text("1-2".into())
    );
    // Multi-arg min/max are scalar, not aggregate arity errors.
    assert_eq!(
        one("SELECT max(a, b, 99) FROM t LIMIT 1"),
        Value::Integer(99)
    );
}

/// The expected `wrong number of arguments` message for the aggregate named in
/// `sql` (the first `name(` token).
fn alloc_name(sql: &str) -> String {
    let open = sql.find('(').unwrap();
    let start = sql[..open]
        .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .map(|i| i + 1)
        .unwrap_or(0);
    let name = &sql[start..open];
    format!("wrong number of arguments to function {name}()")
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
        "CREATE TABLE t(a); SELECT sum() FROM t GROUP BY a",
        "CREATE TABLE t(a); SELECT a FROM t GROUP BY a HAVING sum()",
        "CREATE TABLE t(a); SELECT a FROM t GROUP BY a HAVING avg()",
        "CREATE TABLE t(a); SELECT a FROM t ORDER BY sum()",
        "CREATE TABLE t(a); SELECT a FROM t ORDER BY sum(a, a)",
        "CREATE TABLE t(a); SELECT a FROM t ORDER BY count(1, 2)",
        "CREATE TABLE t(a); SELECT avg(a, a) FROM t GROUP BY a",
        "CREATE TABLE t(a); SELECT a FROM t GROUP BY sum()",
        "CREATE TABLE t(a); SELECT a FROM t WHERE sum()",
        "CREATE TABLE t(a); SELECT a FROM t WHERE sum(a, a)",
        "CREATE TABLE t(a); SELECT sum(count(1, 2)) FROM t",
        // Legitimate forms still compute / report the right value.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,10),(2,20); SELECT sum(a), count(*), count(a), count(), total(a), group_concat(a), group_concat(a,'-') FROM t",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,10),(2,20); SELECT json_group_object(a,b) FROM t",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,10),(2,20); SELECT max(a, b, 99) FROM t LIMIT 1",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
