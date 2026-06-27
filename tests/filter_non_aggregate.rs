//! A `FILTER (WHERE …)` clause is only meaningful on an aggregate call (it
//! restricts which rows feed the aggregation). Attached to an ordinary scalar
//! function — `abs(a) FILTER(WHERE …)` — it is a misuse: SQLite rejects it at
//! *prepare* time with `FILTER may not be used with non-aggregate NAME()`
//! (the name echoed exactly as written). graphite previously parsed the clause
//! and silently ignored it, returning the bare function value. A `FILTER` on a
//! real aggregate, or on a window-aggregate (`sum(a) FILTER(…) OVER ()`), stays
//! legal. Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run through the method that accepts the statement and return its error text.
fn err_of(c: &mut Connection, sql: &str) -> String {
    let r = if sql.trim_start()[..6].eq_ignore_ascii_case("select") {
        c.query(sql).map(|_| ())
    } else {
        c.execute(sql).map(|_| ())
    };
    r.unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn filter_on_non_aggregate_is_misuse() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    // Empty table — the rejection is at prepare time, not per row. The name is
    // echoed exactly as written (case preserved).
    for (sql, name) in [
        ("SELECT abs(a) FILTER(WHERE a > 0) FROM t", "abs"),
        ("SELECT UPPER('x') FILTER(WHERE 1) FROM t", "UPPER"),
        ("SELECT a FROM t WHERE abs(a) FILTER(WHERE 1) > 0", "abs"),
        ("SELECT a FROM t ORDER BY abs(a) FILTER(WHERE 1)", "abs"),
        (
            "SELECT a FROM t GROUP BY a HAVING abs(a) FILTER(WHERE 1) > 0",
            "abs",
        ),
        (
            "SELECT * FROM t JOIN t u ON abs(t.a) FILTER(WHERE 1) > 0",
            "abs",
        ),
        ("UPDATE t SET a = abs(a) FILTER(WHERE 1)", "abs"),
        ("DELETE FROM t WHERE abs(a) FILTER(WHERE 1) > 0", "abs"),
    ] {
        assert_eq!(
            err_of(&mut c, sql),
            format!("FILTER may not be used with non-aggregate {name}()"),
            "for {sql}"
        );
    }

    // Legitimate: FILTER on a real aggregate, and on a window-aggregate.
    c.execute("INSERT INTO t VALUES(1, 2), (3, 4)").unwrap();
    c.query("SELECT sum(a) FILTER(WHERE a > 0) FROM t").unwrap();
    c.query("SELECT sum(a) FILTER(WHERE a > 0) OVER () FROM t")
        .unwrap();
    c.query("SELECT count(*) FILTER(WHERE b > 2) FROM t")
        .unwrap();
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let full = format!("CREATE TABLE t(a, b); INSERT INTO t VALUES(1,2),(3,4); {sql}");
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
    for sql in [
        // misuse — rejected at prepare time in both
        "SELECT abs(a) FILTER(WHERE a > 0) FROM t",
        "SELECT UPPER('x') FILTER(WHERE 1) FROM t",
        "SELECT a FROM t WHERE abs(a) FILTER(WHERE 1) > 0",
        "SELECT a FROM t ORDER BY abs(a) FILTER(WHERE 1)",
        "SELECT a FROM t GROUP BY a HAVING abs(a) FILTER(WHERE 1) > 0",
        "SELECT * FROM t JOIN t u ON abs(t.a) FILTER(WHERE 1) > 0",
        "UPDATE t SET a = abs(a) FILTER(WHERE 1)",
        "DELETE FROM t WHERE abs(a) FILTER(WHERE 1) > 0",
        // legitimate — identical output in both
        "SELECT sum(a) FILTER(WHERE a > 0) FROM t",
        "SELECT sum(a) FILTER(WHERE a > 0) OVER () FROM t",
        "SELECT count(*) FILTER(WHERE b > 2) FROM t",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
