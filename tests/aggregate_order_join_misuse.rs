//! An aggregate (or window) function used in a join `ON` predicate, or an
//! aggregate in the `ORDER BY` of a *non-aggregate* query, is a misuse. SQLite
//! rejects both at *prepare* time — so they error even over an empty table —
//! while graphite evaluated them lazily per row and therefore silently accepted
//! the statement over an empty/fully-filtered table (and emitted a non-sqlite
//! message otherwise).
//!
//! The two wordings differ by position: a join `ON` reads `misuse of aggregate
//! function f()` (the function form, since `ON` is never an aggregate-legit
//! context), whereas an `ORDER BY` reads `misuse of aggregate: f()` (the colon
//! form — SQLite resolves `ORDER BY` where aggregates are otherwise allowed). A
//! window function in either position reads `misuse of window function f()`.
//! Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn order_by_aggregate_in_non_aggregate_query_is_misuse() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    // Empty table — the rejection must be at prepare time, not per row.
    for (sql, name) in [
        ("SELECT a FROM t ORDER BY sum(b)", "sum"),
        ("SELECT a FROM t ORDER BY count(*)", "count"),
        ("SELECT a FROM t ORDER BY max(b) + 1", "max"),
    ] {
        assert_eq!(
            c.query(sql).unwrap_err().to_string(),
            format!("error: misuse of aggregate: {name}()"),
            "for {sql}"
        );
    }
    // Legitimate: an aggregate query may order by an aggregate; a window function
    // in ORDER BY is allowed; a plain ORDER BY is fine.
    c.execute("INSERT INTO t VALUES(1, 2), (3, 4)").unwrap();
    c.query("SELECT sum(a) FROM t ORDER BY sum(b)").unwrap();
    c.query("SELECT a FROM t GROUP BY a ORDER BY sum(b)")
        .unwrap();
    c.query("SELECT a FROM t ORDER BY row_number() OVER ()")
        .unwrap();
    c.query("SELECT a FROM t ORDER BY a DESC").unwrap();
}

#[test]
fn join_on_aggregate_or_window_is_misuse() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    // Empty table — rejected at prepare time.
    assert_eq!(
        c.query("SELECT * FROM t JOIN t u ON sum(t.a) > 0")
            .unwrap_err()
            .to_string(),
        "error: misuse of aggregate function sum()",
    );
    assert_eq!(
        c.query("SELECT * FROM t LEFT JOIN t u ON count(*) > 0")
            .unwrap_err()
            .to_string(),
        "error: misuse of aggregate function count()",
    );
    assert_eq!(
        c.query("SELECT * FROM t JOIN t u ON row_number() OVER () > 0")
            .unwrap_err()
            .to_string(),
        "error: misuse of window function row_number()",
    );
    // Same over a populated table — still the function form, not the lazy message.
    c.execute("INSERT INTO t VALUES(1, 2), (3, 4)").unwrap();
    assert_eq!(
        c.query("SELECT * FROM t JOIN t u ON max(t.b) > 0")
            .unwrap_err()
            .to_string(),
        "error: misuse of aggregate function max()",
    );
    // Legitimate join predicates still run.
    c.query("SELECT * FROM t JOIN t u ON t.a = u.a").unwrap();
    c.query("SELECT * FROM t JOIN t u ON t.a = u.a AND t.b > 0")
        .unwrap();
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, setup: &str, sql: &str| -> String {
        let full = format!("{setup} {sql}");
        let out = Command::new(bin)
            .arg(":memory:")
            .arg(&full)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .trim_end_matches('(')
            .trim_end()
            .to_string()
    };
    let empty = "CREATE TABLE t(a, b);";
    let full = "CREATE TABLE t(a, b); INSERT INTO t VALUES(1,2),(3,4);";
    for (setup, sql) in [
        // ORDER BY aggregate misuse — empty and populated
        (empty, "SELECT a FROM t ORDER BY sum(b)"),
        (full, "SELECT a FROM t ORDER BY sum(b)"),
        (empty, "SELECT a FROM t ORDER BY count(*)"),
        (full, "SELECT a FROM t ORDER BY max(b) + 1"),
        // join ON aggregate/window misuse — empty and populated
        (empty, "SELECT * FROM t JOIN t u ON sum(t.a) > 0"),
        (full, "SELECT * FROM t JOIN t u ON sum(t.a) > 0"),
        (empty, "SELECT * FROM t LEFT JOIN t u ON count(*) > 0"),
        (full, "SELECT * FROM t JOIN t u ON row_number() OVER () > 0"),
        // legitimate — runs in both
        (full, "SELECT sum(a) FROM t ORDER BY sum(b)"),
        (full, "SELECT a FROM t GROUP BY a ORDER BY sum(b)"),
        (full, "SELECT a FROM t ORDER BY row_number() OVER ()"),
        (full, "SELECT count(*) FROM t JOIN t u ON t.a = u.a"),
    ] {
        assert_eq!(
            run("sqlite3", setup, sql),
            run(g, setup, sql),
            "for {setup} {sql}"
        );
    }
}
