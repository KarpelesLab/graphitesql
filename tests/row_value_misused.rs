//! A row value `(a, b, …)` is only legal in the contexts that accept one — a
//! row comparison (`(a,b) = (c,d)`, `(a,b) < (c,d)`) or `(a,b) IN (…)`. Used
//! anywhere a single scalar is expected (bare in the SELECT list, as an
//! arithmetic operand, a function argument, or a bare `WHERE` predicate), SQLite
//! reports `row value misused`. graphite previously used its own wording
//! ("row value used where a single value is expected"). Matched to the `sqlite3`
//! CLI (3.50.4).
//!
//! A *multi-column subquery* `(SELECT x, y)` is the same kind of row value. As a
//! comparison operand (`=`, `<>`, `<`, …, `IS`, `BETWEEN`) against an operand of
//! a different width — a scalar, or a vector of another arity — SQLite also says
//! `row value misused`, not the `sub-select returns N columns - expected 1`
//! column-count message it keeps for genuinely scalar contexts (bare SELECT
//! list, `IN`, function arguments). When both operands are vectors of equal
//! arity the comparison is a lexicographic row comparison (`(SELECT 1,2) =
//! (SELECT 3,4)` is `0`). graphite previously reported the column-count message
//! in every multi-column-subquery context; these are matched here too.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
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

#[test]
fn row_value_in_scalar_context_is_misused() {
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT (1, 2)",
        "SELECT (1, 2, 3)",
        "SELECT (1, 2) + 1",
        "SELECT 1 WHERE (1, 2)",
        "SELECT 1 WHERE (1, 2) = 1",
        "SELECT max((1, 2))",
        "SELECT abs((1, 2))",
    ] {
        assert_eq!(graphite_err(&c, sql), "row value misused", "for {sql}");
    }

    // The legal row-value contexts are unaffected.
    assert_eq!(
        c.query("SELECT (1, 2) = (1, 2)").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("SELECT (1, 2) < (3, 4)").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("SELECT (1, 2) IN ((1, 2), (3, 4))").unwrap().rows[0][0],
        Value::Integer(1)
    );
}

/// A multi-column subquery used as a comparison operand against a different
/// width is `row value misused`; against an equal-width vector it row-compares.
#[test]
fn multicol_subquery_in_comparison() {
    let c = Connection::open_memory().unwrap();
    // Vector-vs-scalar (and vector-vs-vector of unequal arity): row value misused.
    for sql in [
        "SELECT (SELECT 1, 2) = 1",
        "SELECT 1 = (SELECT 1, 2)",
        "SELECT (SELECT 1, 2) <= 1",
        "SELECT (SELECT 1, 2) <> 1",
        "SELECT (SELECT 1, 2) IS 1",
        "SELECT (SELECT 1, 2) IS NOT 1",
        "SELECT (SELECT 1, 2) = (SELECT 3)",
        "SELECT (SELECT 1, 2, 3) = (SELECT 3, 4)",
        "SELECT (SELECT 1, 2) BETWEEN 1 AND 2",
        "SELECT (SELECT 1, 2) NOT BETWEEN 1 AND 2",
        "SELECT 1 BETWEEN (SELECT 1, 2) AND 3",
        "SELECT 1 WHERE (SELECT 1, 2) = 1",
        "SELECT CASE WHEN (SELECT 1, 2) = 1 THEN 1 END",
        "SELECT (VALUES (1, 2)) = 1",
        // A literal row value against a vector of another arity is the same.
        "SELECT (1, 2) = (3, 4, 5)",
    ] {
        assert_eq!(graphite_err(&c, sql), "row value misused", "for {sql}");
    }

    // Equal-arity vectors row-compare (no error); empty subquery acts per SQLite.
    for (sql, want) in [
        ("SELECT (SELECT 1, 2) = (SELECT 3, 4)", Value::Integer(0)),
        ("SELECT (SELECT 1, 2) < (SELECT 3, 4)", Value::Integer(1)),
        ("SELECT (SELECT 1, 2) = (SELECT 1, 2)", Value::Integer(1)),
        ("SELECT (SELECT 1, 2) IS (SELECT 3, 4)", Value::Integer(0)),
        ("SELECT (SELECT 1, 2) IS (SELECT 1, 2)", Value::Integer(1)),
        (
            "SELECT (SELECT 1, 2) BETWEEN (SELECT 1, 2) AND (SELECT 4, 5)",
            Value::Integer(1),
        ),
        // An empty subquery is a row of NULLs: `=` is NULL, `IS` is a real 0/1.
        (
            "SELECT (SELECT 1, 2 WHERE 0) IS (SELECT 3, 4)",
            Value::Integer(0),
        ),
    ] {
        assert_eq!(c.query(sql).unwrap().rows[0][0], want, "for {sql}");
    }
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let sqlite_err = |sql: &str| -> String {
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
    };
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT (1, 2)",
        "SELECT (1, 2, 3)",
        "SELECT (1, 2) + 1",
        "SELECT 1 WHERE (1, 2)",
        "SELECT 1 WHERE (1, 2) = 1",
        "SELECT max((1, 2))",
        "SELECT abs((1, 2))",
        // Multi-column subquery / `VALUES` operands: row value misused as a
        // comparison operand, but the column-count message stays in scalar
        // contexts (bare SELECT list, `IN`, function argument).
        "SELECT (SELECT 1, 2) = 1",
        "SELECT 1 = (SELECT 1, 2)",
        "SELECT (SELECT 1, 2) <> 1",
        "SELECT (SELECT 1, 2) IS 1",
        "SELECT (SELECT 1, 2) = (SELECT 3)",
        "SELECT (SELECT 1, 2, 3) = (SELECT 3, 4)",
        "SELECT (SELECT 1, 2) BETWEEN 1 AND 2",
        "SELECT 1 BETWEEN (SELECT 1, 2) AND 3",
        "SELECT (VALUES (1, 2)) = 1",
        "SELECT (1, 2) = (3, 4, 5)",
        "SELECT (SELECT 1, 2)",
        "SELECT (SELECT 1, 2) IN (1, 2)",
        "SELECT (SELECT 1, 2) + 1",
        "SELECT (SELECT 1, 2) IS NULL",
    ] {
        assert_eq!(graphite_err(&c, sql), sqlite_err(sql), "for {sql}");
    }

    // Equal-arity vector comparisons return a value — compare full CLI output.
    let sqlite_val = |sql: &str| -> String {
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg(sql)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    };
    for sql in [
        "SELECT (SELECT 1, 2) = (SELECT 3, 4)",
        "SELECT (SELECT 1, 2) < (SELECT 3, 4)",
        "SELECT (SELECT 1, 2) IS (SELECT 1, 2)",
        "SELECT (VALUES (1, 2)) = (VALUES (3, 4))",
        "SELECT (1, 2, 3) BETWEEN (1, 2, 3) AND (4, 5, 6)",
        "SELECT (SELECT 1, 2) BETWEEN (SELECT 1, 2) AND (SELECT 4, 5)",
    ] {
        let got = match c.query(sql).unwrap().rows[0][0].clone() {
            Value::Integer(i) => i.to_string(),
            Value::Null => String::new(),
            other => panic!("unexpected {other:?} for {sql}"),
        };
        assert_eq!(got, sqlite_val(sql), "for {sql}");
    }
}
