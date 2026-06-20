//! Tests for the `timediff(A, B)` scalar function.
//!
//! `timediff` returns the calendar time difference from B to A, formatted as
//! `(+|-)YYYY-MM-DD HH:MM:SS.SSS`. The expected strings here were produced by
//! the real `sqlite3` CLI; a differential pass re-checks them when the CLI is
//! present.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite_eval(expr: &str) -> Option<String> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("SELECT quote({expr});"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Render a value the way `sqlite3`'s `quote()` would, so NULL is distinguishable
/// from an empty string.
fn quote(v: &Value) -> String {
    match v {
        Value::Null => String::from("NULL"),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => format!("{r}"),
        Value::Blob(b) => {
            let mut s = String::from("X'");
            for x in b {
                s.push_str(&format!("{x:02X}"));
            }
            s.push('\'');
            s
        }
    }
}

fn eval(conn: &Connection, expr: &str) -> String {
    let r = conn.query(&format!("SELECT {expr}")).unwrap();
    quote(&r.rows[0][0])
}

#[test]
fn timediff_known_values() {
    let conn = Connection::open_memory().unwrap();
    // (expression, expected sqlite3 output)
    let cases = [
        (
            "timediff('2020-01-02 00:00:00','2020-01-01 00:00:00')",
            "'+0000-00-01 00:00:00.000'",
        ),
        (
            "timediff('2024-03-01','2024-02-01')",
            "'+0000-01-00 00:00:00.000'",
        ),
        (
            "timediff('2020-01-01','2020-03-01')",
            "'-0000-02-00 00:00:00.000'",
        ),
        (
            "timediff('2025-06-15','2020-01-01')",
            "'+0005-05-14 00:00:00.000'",
        ),
        (
            "timediff('2020-01-01 12:30:45.5','2020-01-01 12:00:00')",
            "'+0000-00-00 00:30:45.500'",
        ),
        (
            "timediff('2020-01-01','2020-01-01')",
            "'+0000-00-00 00:00:00.000'",
        ),
        // sub-second, negative sign
        (
            "timediff('2020-01-01 12:00:00','2020-01-01 12:30:45.5')",
            "'-0000-00-00 00:30:45.500'",
        ),
        (
            "timediff('2000-01-01 00:00:00','2000-01-01 00:00:00.500')",
            "'-0000-00-00 00:00:00.500'",
        ),
        // crossing a year boundary by one second
        (
            "timediff('2024-01-01','2023-12-31 23:59:59')",
            "'+0000-00-00 00:00:01.000'",
        ),
        // leap-day arithmetic (2020 and 2024 are leap years)
        (
            "timediff('2020-02-29','2020-02-28')",
            "'+0000-00-01 00:00:00.000'",
        ),
        (
            "timediff('2020-02-28','2020-02-29')",
            "'-0000-00-01 00:00:00.000'",
        ),
        (
            "timediff('2024-02-29 12:00:00','2024-02-28 06:00:00')",
            "'+0000-00-01 06:00:00.000'",
        ),
        (
            "timediff('2020-02-29','2019-03-01')",
            "'+0000-11-28 00:00:00.000'",
        ),
        // month-length boundaries (the month back-off loop)
        (
            "timediff('2024-03-31','2024-02-29')",
            "'+0000-01-02 00:00:00.000'",
        ),
        (
            "timediff('2020-03-31','2020-02-29')",
            "'+0000-01-02 00:00:00.000'",
        ),
        (
            "timediff('2021-03-31','2021-02-28')",
            "'+0000-01-03 00:00:00.000'",
        ),
        (
            "timediff('2024-03-30','2024-01-31')",
            "'+0000-01-28 00:00:00.000'",
        ),
        // within-month, negative
        (
            "timediff('2020-01-15','2020-01-31')",
            "'-0000-00-16 00:00:00.000'",
        ),
        // large spans (year padding stays 4 digits)
        (
            "timediff('2025-12-31 23:59:59.999','2025-01-01 00:00:00.000')",
            "'+0000-11-30 23:59:59.999'",
        ),
        (
            "timediff('9999-12-31','2000-01-01')",
            "'+7999-11-30 00:00:00.000'",
        ),
        (
            "timediff('2000-01-01','9999-12-31')",
            "'-7999-11-30 00:00:00.000'",
        ),
        // NULL / invalid input -> NULL
        ("timediff('not a date','2020-01-01')", "NULL"),
        ("timediff('2020-01-01','garbage')", "NULL"),
        ("timediff(NULL,'2020-01-01')", "NULL"),
        ("timediff('2020-01-01',NULL)", "NULL"),
    ];

    for (expr, want) in cases {
        assert_eq!(eval(&conn, expr), want, "graphite mismatch for {expr}");
    }
}

/// Re-verify every case against the live `sqlite3` CLI when it is installed.
#[test]
fn timediff_differential() {
    if !sqlite_available() {
        eprintln!("sqlite3 CLI not found; skipping timediff differential");
        return;
    }
    let conn = Connection::open_memory().unwrap();
    let exprs = [
        "timediff('2020-01-02 00:00:00','2020-01-01 00:00:00')",
        "timediff('2024-03-01','2024-02-01')",
        "timediff('2020-01-01','2020-03-01')",
        "timediff('2025-06-15','2020-01-01')",
        "timediff('2020-01-01 12:30:45.5','2020-01-01 12:00:00')",
        "timediff('2020-01-01','2020-01-01')",
        "timediff('2020-01-01 12:00:00','2020-01-01 12:30:45.5')",
        "timediff('2000-01-01 00:00:00','2000-01-01 00:00:00.500')",
        "timediff('2024-01-01','2023-12-31 23:59:59')",
        "timediff('2020-02-29','2020-02-28')",
        "timediff('2020-02-28','2020-02-29')",
        "timediff('2024-02-29 12:00:00','2024-02-28 06:00:00')",
        "timediff('2020-02-29','2019-03-01')",
        "timediff('2024-03-31','2024-02-29')",
        "timediff('2020-03-31','2020-02-29')",
        "timediff('2021-03-31','2021-02-28')",
        "timediff('2024-03-30','2024-01-31')",
        "timediff('2020-01-15','2020-01-31')",
        "timediff('2025-12-31 23:59:59.999','2025-01-01 00:00:00.000')",
        "timediff('9999-12-31','2000-01-01')",
        "timediff('2000-01-01','9999-12-31')",
        "timediff('not a date','2020-01-01')",
        "timediff('2020-01-01','garbage')",
        "timediff(NULL,'2020-01-01')",
        "timediff('2020-01-01',NULL)",
    ];

    let mut failures = Vec::new();
    for e in exprs {
        let Some(want) = sqlite_eval(e) else { continue };
        let got = eval(&conn, e);
        if got != want {
            failures.push(format!("  {e}\n    sqlite:   {want}\n    graphite: {got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} timediff expressions diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
