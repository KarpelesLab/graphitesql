//! `unistr()`, `unistr_quote()` and `subtype()` — string builtins added to
//! SQLite core in 3.50. `unistr` decodes `\uXXXX`/`\UXXXXXXXX`/`\\` escapes;
//! `unistr_quote` is `quote()` except control-bearing text renders as
//! `unistr('…')`. Behaviour verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn val(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}

#[test]
fn unistr_decodes_escapes() {
    let c = Connection::open_memory().unwrap();
    // A 4-digit \u escape (the SQL text is aéb, which decodes to aéb).
    assert_eq!(
        val(&c, "SELECT unistr('a\\u00e9b')"),
        Value::Text("aéb".into())
    );
    // 8-digit \U reaches astral code points.
    assert_eq!(
        val(&c, r"SELECT unistr('\U0001F600')"),
        Value::Text("😀".into())
    );
    // `\\` is a literal backslash; other text passes through.
    assert_eq!(
        val(&c, r"SELECT unistr('back\\slash')"),
        Value::Text(r"back\slash".into())
    );
    // Non-text coerces to its text form first; NULL stays NULL.
    assert_eq!(val(&c, "SELECT unistr(123)"), Value::Text("123".into()));
    assert_eq!(val(&c, "SELECT unistr(NULL)"), Value::Null);
    // \u equals the same code point via char().
    assert_eq!(
        val(&c, "SELECT unistr('\\u00e9') = char(233)"),
        Value::Integer(1)
    );
}

#[test]
fn unistr_rejects_bad_escapes() {
    let c = Connection::open_memory().unwrap();
    // A backslash must be followed by u/U/\ with the right number of hex digits.
    for bad in [r"SELECT unistr('trailing\u')", r"SELECT unistr('a\nb')"] {
        assert!(c.query(bad).is_err(), "{bad} should error");
    }
}

#[test]
fn unistr_quote_matches_quote_except_for_control_chars() {
    let c = Connection::open_memory().unwrap();
    // No control characters: identical to quote() (non-ASCII kept literal).
    assert_eq!(
        val(&c, "SELECT unistr_quote('café')"),
        Value::Text("'café'".into())
    );
    assert_eq!(
        val(&c, "SELECT unistr_quote('a''b')"),
        Value::Text("'a''b'".into())
    );
    // A control character switches to the unistr('…') form, with the tab
    // escaped as the six literal characters backslash-u-0-0-0-9.
    assert_eq!(
        val(&c, "SELECT unistr_quote('a' || char(9) || 'b')"),
        Value::Text("unistr('a\\u0009b')".into())
    );
    // Non-text values match quote() exactly.
    assert_eq!(
        val(&c, "SELECT unistr_quote(NULL)"),
        Value::Text("NULL".into())
    );
    assert_eq!(
        val(&c, "SELECT unistr_quote(123)"),
        Value::Text("123".into())
    );
    assert_eq!(
        val(&c, "SELECT unistr_quote(x'4142')"),
        Value::Text("X'4142'".into())
    );
}

#[test]
fn subtype_is_zero() {
    let c = Connection::open_memory().unwrap();
    // graphite tracks no value subtypes, so subtype() is always 0 (as SQLite is
    // for ordinary values), including for NULL.
    for q in [
        "SELECT subtype(1)",
        "SELECT subtype(NULL)",
        "SELECT subtype('x')",
    ] {
        assert_eq!(val(&c, q), Value::Integer(0), "{q}");
    }
}
