//! Track A6: the `json_error_position(X)` scalar function.
//!
//! Reports the 1-based byte position of the first JSON syntax error in `X`, or 0
//! when `X` is well-formed JSON. Positions are matched against the `sqlite3` CLI
//! for the common malformed shapes (see `divergences` for the known edge cases).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn pos(c: &Connection, json: &str) -> Value {
    let sql = format!("SELECT json_error_position('{}')", json.replace('\'', "''"));
    c.query(&sql).unwrap().rows.remove(0).remove(0)
}

fn n(c: &Connection, json: &str) -> i64 {
    match pos(c, json) {
        Value::Integer(i) => i,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn valid_json_is_zero() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(n(&c, r#"{"ok":1}"#), 0);
    assert_eq!(n(&c, "[1,2,3]"), 0);
    assert_eq!(n(&c, "null"), 0);
    assert_eq!(n(&c, "true"), 0);
    assert_eq!(n(&c, "123"), 0);
    assert_eq!(n(&c, "1.5"), 0);
    assert_eq!(n(&c, r#""hello""#), 0);
    assert_eq!(n(&c, r#"{"a":{"b":[1,2,{"c":3}]}}"#), 0);
    // Leading/trailing whitespace is allowed.
    assert_eq!(n(&c, "  [1,2]  "), 0);
}

#[test]
fn malformed_positions_match_sqlite3() {
    let c = Connection::open_memory().unwrap();
    // Task examples.
    assert_eq!(n(&c, "{bad"), 5);
    assert_eq!(n(&c, "[1,2,}"), 6);
    // Truncated containers (EOF reached).
    assert_eq!(n(&c, "[1,2"), 5);
    assert_eq!(n(&c, r#"{"a":1,"#), 8);
    assert_eq!(n(&c, "["), 2);
    assert_eq!(n(&c, "{"), 2);
    // Missing separators / colons.
    assert_eq!(n(&c, r#"{"a"}"#), 5);
    assert_eq!(n(&c, "[1 2]"), 4);
    assert_eq!(n(&c, r#"{"a":1 "b":2}"#), 8);
    // Bad token in value position.
    assert_eq!(n(&c, r#"{"a":}"#), 6);
    assert_eq!(n(&c, "[,]"), 2);
    assert_eq!(n(&c, "[1,,2]"), 4);
    assert_eq!(n(&c, r#"{"a":{"b":}}"#), 11);
    // Bare-identifier key (JSON5-style) is scanned to its end.
    assert_eq!(n(&c, r#"{"a":1,bad}"#), 11);
    // Unterminated strings.
    assert_eq!(n(&c, "\"unterminated"), 14);
    assert_eq!(n(&c, "\"a"), 3);
    // Empty / whitespace-only / pure junk all report position 1.
    assert_eq!(n(&c, ""), 1);
    assert_eq!(n(&c, "   "), 1);
    assert_eq!(n(&c, "abc"), 1);
    assert_eq!(n(&c, "tru"), 1);
    // A leading zero on an integer part is malformed; sqlite points at the
    // token's second character (`start + 1`), sign or not.
    assert_eq!(n(&c, "00"), 2);
    assert_eq!(n(&c, "007"), 2);
    assert_eq!(n(&c, "000"), 2);
    assert_eq!(n(&c, "00.5"), 2);
    assert_eq!(n(&c, "-01"), 2);
    assert_eq!(n(&c, "[01]"), 3);
    assert_eq!(n(&c, "[1,00]"), 5);
    assert_eq!(n(&c, r#"{"a":01}"#), 7);
}

#[test]
fn null_argument_is_null() {
    let c = Connection::open_memory().unwrap();
    let v = c
        .query("SELECT json_error_position(NULL)")
        .unwrap()
        .rows
        .remove(0)
        .remove(0);
    assert_eq!(v, Value::Null);
}

#[test]
fn numeric_argument_uses_text_form() {
    let c = Connection::open_memory().unwrap();
    // 123 -> "123" is valid JSON -> 0.
    let v = c
        .query("SELECT json_error_position(123)")
        .unwrap()
        .rows
        .remove(0)
        .remove(0);
    assert_eq!(v, Value::Integer(0));
}

#[test]
fn arity_error() {
    let c = Connection::open_memory().unwrap();
    assert!(c.query("SELECT json_error_position()").is_err());
    assert!(c.query("SELECT json_error_position('[1]', '[2]')").is_err());
}
