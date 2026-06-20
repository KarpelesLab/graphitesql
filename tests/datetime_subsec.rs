//! The `subsec` / `subsecond` modifier, matched to the `sqlite3` CLI (3.50.4).
//!
//! It makes `datetime()` and `time()` render seconds with three fractional
//! digits (`SS.SSS`). It does not affect `date()` (no time part) nor explicit
//! `strftime()` format strings.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn t(c: &Connection, sql: &str) -> String {
    match c.query(sql).unwrap().rows.remove(0).remove(0) {
        Value::Text(s) => s,
        other => panic!("expected text from {sql}, got {other:?}"),
    }
}

#[test]
fn subsec_renders_milliseconds() {
    let c = Connection::open_memory().unwrap();
    // Whole seconds gain `.000`; a fractional input is preserved to 3 digits.
    assert_eq!(
        t(&c, "SELECT datetime('2024-01-01 12:30:45','subsec')"),
        "2024-01-01 12:30:45.000"
    );
    assert_eq!(
        t(&c, "SELECT datetime('2024-01-01 12:30:45.678','subsec')"),
        "2024-01-01 12:30:45.678"
    );
    // `subsecond` is an accepted spelling.
    assert_eq!(
        t(&c, "SELECT datetime('2024-01-01 12:30:45.678','subsecond')"),
        "2024-01-01 12:30:45.678"
    );
    assert_eq!(
        t(&c, "SELECT time('2024-01-01 12:30:45.5','subsec')"),
        "12:30:45.500"
    );
    assert_eq!(t(&c, "SELECT time('12:30:45','subsec')"), "12:30:45.000");
    // A fractional offset shows through.
    assert_eq!(
        t(&c, "SELECT datetime('2024-01-01','subsec','+0.25 seconds')"),
        "2024-01-01 00:00:00.250"
    );
}

#[test]
fn subsec_does_not_affect_date_or_strftime() {
    let c = Connection::open_memory().unwrap();
    // `date()` has no time part, so `subsec` is a no-op.
    assert_eq!(
        t(&c, "SELECT date('2024-01-01 12:30:45.678','subsec')"),
        "2024-01-01"
    );
    // Explicit strftime formats are unchanged by `subsec`.
    assert_eq!(
        t(
            &c,
            "SELECT strftime('%H:%M','2024-01-01 12:30:45','subsec')"
        ),
        "12:30"
    );
    assert_eq!(
        t(
            &c,
            "SELECT strftime('%S','2024-01-01 12:30:45.678','subsec')"
        ),
        "45"
    );
    // Without the modifier, datetime() stays whole-second even with fractional input.
    assert_eq!(
        t(&c, "SELECT datetime('2024-01-01 12:30:45.678')"),
        "2024-01-01 12:30:45"
    );
}
