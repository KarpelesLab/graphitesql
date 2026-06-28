//! `GROUP BY <n>` groups by the n-th output column (not the constant n), and
//! `generate_series(..,0)` treats a zero step as 1 — both matched to the sqlite3
//! CLI. (Tests use ORDER BY so the row order is deterministic; SQLite's grouped
//! output order without ORDER BY is unspecified and graphite may differ.)

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

#[test]
fn group_by_position_resolves_to_column() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(1),(2),(3),(4)")
        .unwrap();
    // GROUP BY 1 groups by `a` (per-value counts), not by the constant 1 (which
    // would collapse to a single group of 5).
    assert_eq!(
        rows(&c, "SELECT a, count(*) FROM t GROUP BY 1 ORDER BY 1"),
        vec![
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(2), Value::Integer(1)],
            vec![Value::Integer(3), Value::Integer(1)],
            vec![Value::Integer(4), Value::Integer(1)],
        ]
    );
    // The position names the output *expression*, so an expression column works.
    assert_eq!(
        rows(&c, "SELECT a%2 p, count(*) FROM t GROUP BY 1 ORDER BY 1"),
        vec![
            vec![Value::Integer(0), Value::Integer(2)],
            vec![Value::Integer(1), Value::Integer(3)],
        ]
    );
    // GROUP BY 1 and GROUP BY a agree.
    assert_eq!(
        rows(&c, "SELECT a, count(*) FROM t GROUP BY 1 ORDER BY 1"),
        rows(&c, "SELECT a, count(*) FROM t GROUP BY a ORDER BY a")
    );
}

/// An out-of-range positional `ORDER BY` / `GROUP BY` term reports SQLite's exact
/// message body: `<ordinal> <clause> term out of range - should be between 1 and
/// <ncols>`, with the ordinal being the offending term's 1-based position within
/// its clause. SQLite resolves ORDER BY before GROUP BY, so a both-out-of-range
/// query reports the ORDER BY term. (The CLI's "Parse error" / "Error:" wrapper
/// differs and is normalized out by the differential corpus — the body matches.)
#[test]
fn positional_term_out_of_range_message_matches_sqlite() {
    let c = Connection::open_memory().unwrap();
    // Strip the generic `error: ` Display prefix; we assert on the message body,
    // which is what the differential corpus compares against sqlite3.
    let err = |sql: &str| {
        c.query(sql)
            .unwrap_err()
            .to_string()
            .trim_start_matches("error: ")
            .to_string()
    };

    assert_eq!(
        err("SELECT 1 ORDER BY 2"),
        "1st ORDER BY term out of range - should be between 1 and 1"
    );
    assert_eq!(
        err("SELECT 1,2,3 ORDER BY 5"),
        "1st ORDER BY term out of range - should be between 1 and 3"
    );
    // The ordinal counts every term, positional or not: term 2 (`5`) is the bad one.
    assert_eq!(
        err("SELECT 1 AS a ORDER BY a, 5"),
        "2nd ORDER BY term out of range - should be between 1 and 1"
    );
    assert_eq!(
        err("SELECT 1 ORDER BY 1, 1, 4"),
        "3rd ORDER BY term out of range - should be between 1 and 1"
    );
    // GROUP BY clause + its own ordinal numbering.
    assert_eq!(
        err("SELECT 1,2 GROUP BY 3"),
        "1st GROUP BY term out of range - should be between 1 and 2"
    );
    assert_eq!(
        err("SELECT 1,2 GROUP BY 1, 3"),
        "2nd GROUP BY term out of range - should be between 1 and 2"
    );
    // ORDER BY is resolved before GROUP BY: both bad → ORDER BY reported.
    assert_eq!(
        err("SELECT 1 a, 2 b GROUP BY 9 ORDER BY 8"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
    // Only GROUP BY bad → GROUP BY reported.
    assert_eq!(
        err("SELECT 1 a, 2 b GROUP BY 9 ORDER BY 1"),
        "1st GROUP BY term out of range - should be between 1 and 2"
    );
    // Unary `+` is a parser no-op, so `+2` is positional 2 (out of range).
    assert_eq!(
        err("SELECT 1 ORDER BY +2"),
        "1st ORDER BY term out of range - should be between 1 and 1"
    );
    // Teen vs. twenty-first ordinal suffixes (`%r`: 13th, 21st).
    let many = |n: usize| core::iter::repeat_n("1", n).collect::<Vec<_>>().join(",");
    assert_eq!(
        err(&format!("SELECT {} ORDER BY {},99", many(12), many(12))),
        "13th ORDER BY term out of range - should be between 1 and 12"
    );
    assert_eq!(
        err(&format!("SELECT {} ORDER BY {},99", many(21), many(20))),
        "21st ORDER BY term out of range - should be between 1 and 21"
    );

    // A real / text constant is NOT positional (sorts by the constant; no error).
    assert!(c.query("SELECT 1 ORDER BY 1.0").is_ok());
    assert!(c.query("SELECT 1 ORDER BY '2'").is_ok());
}

/// An *in-range* signed / parenthesized / `COLLATE`-wrapped integer in `ORDER BY`
/// is a positional column reference, exactly like a bare integer — SQLite folds
/// the unary sign, so `ORDER BY +1` sorts by output column 1. (Regression: it used
/// to be treated as the constant expression `+1`, a no-op sort that left rows in
/// scan order.)
#[test]
fn signed_positional_order_by_resolves_to_column() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(30, 11), (10, 22), (20, 33)")
        .unwrap();

    let sorted_by_a = vec![
        vec![Value::Integer(10), Value::Integer(22)],
        vec![Value::Integer(20), Value::Integer(33)],
        vec![Value::Integer(30), Value::Integer(11)],
    ];
    // `+1`, `(1)`, `1 COLLATE BINARY`, the bare `1`, and the column name all agree
    // — and all genuinely sort (not the old insertion-order no-op).
    for q in [
        "SELECT a, b FROM t ORDER BY +1",
        "SELECT a, b FROM t ORDER BY (1)",
        "SELECT a, b FROM t ORDER BY 1 COLLATE BINARY",
        "SELECT a, b FROM t ORDER BY 1",
        "SELECT a, b FROM t ORDER BY a",
    ] {
        assert_eq!(rows(&c, q), sorted_by_a, "{q}");
    }

    // The sign composes with ASC/DESC on the resolved column, not the literal.
    assert_eq!(
        rows(&c, "SELECT a, b FROM t ORDER BY +2 DESC"),
        vec![
            vec![Value::Integer(20), Value::Integer(33)],
            vec![Value::Integer(10), Value::Integer(22)],
            vec![Value::Integer(30), Value::Integer(11)],
        ]
    );

    // A compound query: `+2` is positional there too (it used to error with
    // "ORDER BY term does not match any column in the result set").
    assert_eq!(
        rows(
            &c,
            "SELECT a, b FROM t UNION SELECT a, b FROM t ORDER BY +2"
        ),
        rows(&c, "SELECT a, b FROM t UNION SELECT a, b FROM t ORDER BY 2")
    );
}

/// An *in-range* signed / wrapped integer in `GROUP BY` is positional too, so
/// `GROUP BY +1` groups by output column 1 (not the constant `+1`, which would
/// collapse every row into one group).
#[test]
fn signed_positional_group_by_resolves_to_column() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1, 5), (1, 6), (2, 7), (3, 8)")
        .unwrap();

    let grouped = vec![
        vec![Value::Integer(1), Value::Integer(2)],
        vec![Value::Integer(2), Value::Integer(1)],
        vec![Value::Integer(3), Value::Integer(1)],
    ];
    for q in [
        "SELECT a, count(*) FROM t GROUP BY +1 ORDER BY +1",
        "SELECT a, count(*) FROM t GROUP BY (1) ORDER BY 1",
        "SELECT a, count(*) FROM t GROUP BY 1 ORDER BY 1",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY a",
    ] {
        assert_eq!(rows(&c, q), grouped, "{q}");
    }
}

#[test]
fn generate_series_zero_step_is_one() {
    let c = Connection::open_memory().unwrap();
    // A zero step behaves like step 1.
    assert_eq!(
        rows(&c, "SELECT count(*) FROM generate_series(0,5,0)")[0][0],
        Value::Integer(6)
    );
    assert_eq!(
        rows(
            &c,
            "SELECT value FROM generate_series(0,3,0) ORDER BY value"
        ),
        vec![
            vec![Value::Integer(0)],
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );
    // Descending range with a zero (→ positive) step yields nothing.
    assert_eq!(
        rows(&c, "SELECT count(*) FROM generate_series(10,1,0)")[0][0],
        Value::Integer(0)
    );
}
