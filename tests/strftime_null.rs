//! `strftime` unknown-specifier handling, the `%U` week number, and the upper
//! year bound — matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn q(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows.remove(0).remove(0)
}

fn text(c: &Connection, sql: &str) -> String {
    match q(c, sql) {
        Value::Text(s) => s,
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
