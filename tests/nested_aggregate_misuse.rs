//! A nested aggregate — an aggregate function whose argument (or `FILTER`
//! clause) itself contains an aggregate, e.g. `sum(count(a))` — is a misuse: the
//! inner call would have to be evaluated per row, as a scalar, inside the outer
//! aggregation. SQLite reports `misuse of aggregate function NAME()`, naming the
//! inner call. graphite previously emitted its own `aggregate function NAME used
//! outside an aggregate context` wording for the same case. Matched to the
//! `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn nested_aggregate_reports_inner_call() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1, 2), (3, 4)").unwrap();
    for (sql, inner) in [
        ("SELECT sum(count(a)) FROM t", "count"),
        ("SELECT count(sum(a)) FROM t", "sum"),
        ("SELECT max(min(a)) FROM t", "min"),
        ("SELECT sum(a) + count(min(b)) FROM t", "min"),
        ("SELECT count(*) FILTER(WHERE sum(a) > 0) FROM t", "sum"),
        (
            "SELECT sum(a) FROM t GROUP BY b HAVING count(sum(a)) > 0",
            "sum",
        ),
    ] {
        assert_eq!(
            c.query(sql).unwrap_err().to_string(),
            format!("error: misuse of aggregate function {inner}()"),
            "for {sql}"
        );
    }
    // Ordinary (non-nested) aggregates still compute.
    c.query("SELECT sum(a), count(*), max(b) FROM t").unwrap();
    c.query("SELECT a, sum(b) FROM t GROUP BY a").unwrap();
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
        "SELECT sum(count(a)) FROM t",
        "SELECT count(sum(a)) FROM t",
        "SELECT max(min(a)) FROM t",
        "SELECT sum(a) + count(min(b)) FROM t",
        "SELECT count(*) FILTER(WHERE sum(a) > 0) FROM t",
        "SELECT sum(a) FROM t GROUP BY b HAVING count(sum(a)) > 0",
        // ordinary aggregates — identical output in both
        "SELECT sum(a), count(*), max(b) FROM t",
        "SELECT a, sum(b) FROM t GROUP BY a ORDER BY a",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
