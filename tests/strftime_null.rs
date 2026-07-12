//! `strftime` unknown-specifier handling, the `%U` week number, and the upper
//! year bound — matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn q(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows.remove(0).remove(0)
}

fn text(c: &Connection, sql: &str) -> String {
    match q(c, sql) {
        Value::Text(s) => String::from(s.as_str()),
        other => panic!("expected text from {sql}, got {other:?}"),
    }
}

#[test]
fn unknown_specifier_yields_null() {
    let c = Connection::open_memory().unwrap();
    // SQLite aborts the whole conversion to NULL on any specifier it doesn't
    // recognize, rather than emitting the literal `%X`.
    for f in [
        "%c", "A%cB", "%Y-%q", "%-d", "%a", "%A", "%b", "%B", "%D", "%h", "%n", "%r", "%x", "%y",
        "%z", "%Z",
    ] {
        assert_eq!(
            q(&c, &format!("SELECT strftime('{f}','2024-06-15 13:45:30')")),
            Value::Null,
            "{f} should NULL"
        );
    }
    // A literal `%%` is fine and every documented specifier still renders.
    assert_eq!(
        text(&c, "SELECT strftime('100%%done','2024-06-15')"),
        "100%done"
    );
    assert_eq!(
        text(
            &c,
            "SELECT strftime('%Y-%m-%dT%H:%M:%S','2024-06-15 13:45:30')"
        ),
        "2024-06-15T13:45:30"
    );
    assert_eq!(
        text(&c, "SELECT strftime('%j %w %W %V %G %u','2024-06-15')"),
        "167 6 24 24 2024 6"
    );
}

#[test]
fn sunday_week_number_u() {
    let c = Connection::open_memory().unwrap();
    // %U: week of year with Sunday as the first day (00-53); days before the
    // first Sunday are week 00.
    for (date, want) in [
        ("2024-01-01", "00"), // Monday, before the first Sunday
        ("2024-01-06", "00"), // Saturday
        ("2024-01-07", "01"), // first Sunday
        ("2024-01-08", "01"),
        ("2024-12-31", "52"),
        ("2023-01-01", "01"), // Jan 1 is itself a Sunday
    ] {
        assert_eq!(text(&c, &format!("SELECT strftime('%U','{date}')")), want);
    }
}

#[test]
fn no_time_value_defaults_to_now() {
    let c = Connection::open_memory().unwrap();
    // Like `date()`/`time()`/`datetime()`, a `strftime` with only a format
    // argument defaults the time-value to 'now' rather than yielding NULL.
    // We can't pin the exact instant, but it must be non-NULL and shaped right.
    let y = text(&c, "SELECT strftime('%Y')");
    assert_eq!(y.len(), 4, "year should be 4 digits, got {y}");
    assert!(y.chars().all(|ch| ch.is_ascii_digit()), "got {y}");
    let hm = text(&c, "SELECT strftime('%H:%M')");
    assert_eq!(hm.len(), 5, "HH:MM, got {hm}");
    // Agreement with `strftime(fmt,'now')` (computed in the same statement so the
    // clock can't drift across the comparison) at minute granularity.
    assert_eq!(
        q(
            &c,
            "SELECT strftime('%Y-%m-%d %H:%M') = strftime('%Y-%m-%d %H:%M','now')"
        ),
        Value::Integer(1)
    );
}

#[test]
fn non_text_format_is_coerced_to_text() {
    let c = Connection::open_memory().unwrap();
    // SQLite coerces a non-text format argument to text before rendering, so a
    // format with no `%` specifiers prints literally. Only a NULL format -> NULL.
    assert_eq!(text(&c, "SELECT strftime(123)"), "123");
    assert_eq!(text(&c, "SELECT strftime(12.5)"), "12.5");
    assert_eq!(text(&c, "SELECT strftime(123,'2020-01-01')"), "123");
    assert_eq!(text(&c, "SELECT strftime(x'41')"), "A");
    assert_eq!(q(&c, "SELECT strftime(NULL)"), Value::Null);
    assert_eq!(q(&c, "SELECT strftime(NULL,'now')"), Value::Null);
}

#[test]
fn year_past_9999_is_null() {
    let c = Connection::open_memory().unwrap();
    // Running past the end of year 9999 yields NULL across every date function.
    for f in [
        "date('9999-12-31','+1 day')",
        "time('9999-12-31','+1 day')",
        "datetime('9999-12-31 23:59:59','+1 second')",
        "julianday('9999-12-31','+1 day')",
        "strftime('%Y','9999-12-31','+1 day')",
    ] {
        assert_eq!(
            q(&c, &format!("SELECT {f}")),
            Value::Null,
            "{f} should NULL"
        );
    }
    // Year 9999 itself is still valid, and arithmetic into negative years works.
    assert_eq!(text(&c, "SELECT date('9999-12-31')"), "9999-12-31");
    assert_eq!(
        text(&c, "SELECT datetime('9999-12-31 23:59:59')"),
        "9999-12-31 23:59:59"
    );
    assert_eq!(
        text(&c, "SELECT date('0000-01-01','-1 day')"),
        "-0001-12-31"
    );
}
