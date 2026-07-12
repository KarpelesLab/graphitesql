//! The `subsec` / `subsecond` modifier, matched to the `sqlite3` CLI (3.50.4).
//!
//! It makes `datetime()` and `time()` render seconds with three fractional
//! digits (`SS.SSS`), and `strftime('%s', …)` render the epoch with millisecond
//! precision (`<secs>.mmm`). It does not affect `date()` (no time part) nor the
//! field specifiers like `%H`/`%M`/`%S`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn t(c: &Connection, sql: &str) -> String {
    match c.query(sql).unwrap().rows.remove(0).remove(0) {
        Value::Text(s) => String::from(s.as_str()),
        other => panic!("expected text from {sql}, got {other:?}"),
    }
}

#[test]
fn unixepoch_subsec_returns_fractional_real() {
    let c = Connection::open_memory().unwrap();
    // With `subsec`, unixepoch() returns a REAL carrying the fractional second;
    // without it, an INTEGER — matching SQLite.
    let one = |sql: &str| c.query(sql).unwrap().rows.remove(0).remove(0);
    assert_eq!(
        one("SELECT unixepoch('2020-01-01 00:00:00.5','subsec')"),
        Value::Real(1_577_836_800.5)
    );
    assert_eq!(
        one("SELECT unixepoch('2020-01-01 00:00:00.123','subsecond')"),
        Value::Real(1_577_836_800.123)
    );
    // Whole-second input with subsec is still a real (`.0`).
    assert_eq!(
        one("SELECT unixepoch('2020-01-01 00:00:00','subsec')"),
        Value::Real(1_577_836_800.0)
    );
    // No modifier → integer seconds, unchanged.
    assert_eq!(
        one("SELECT unixepoch('2020-01-01')"),
        Value::Integer(1_577_836_800)
    );
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
    // ...but `%s` (epoch seconds) DOES render millisecond precision with subsec.
    assert_eq!(
        t(&c, "SELECT strftime('%s','2024-01-01 00:00:00','subsec')"),
        "1704067200.000"
    );
    assert_eq!(
        t(
            &c,
            "SELECT strftime('%s','2024-01-01 00:00:00.5','subsecond')"
        ),
        "1704067200.500"
    );
    // Without the modifier, `%s` is integer seconds (fraction truncated).
    assert_eq!(
        t(&c, "SELECT strftime('%s','2024-01-01 00:00:00.999')"),
        "1704067200"
    );
    // Without the modifier, datetime() stays whole-second even with fractional input.
    assert_eq!(
        t(&c, "SELECT datetime('2024-01-01 12:30:45.678')"),
        "2024-01-01 12:30:45"
    );
}
