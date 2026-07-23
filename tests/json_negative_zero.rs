//! SQLite preserves the JSON *integer* literal `-0` verbatim: `json('-0')` is
//! `-0` (not `0`), it survives a JSONB round-trip, and `jsonb('-0')` is a
//! distinct blob from `jsonb('0')` (so `jsonb('-0') != jsonb('0')`). graphite
//! used to reparse the integer token to `i64`, collapsing `-0` to `0` and losing
//! the sign. The strict decimal `-0` is stored verbatim under the INT tag — the
//! same tag as `0`/`-5` — never INT5 (which is reserved for JSON5 hex forms).
//!
//! Every expected value below is byte-verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn one(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}
fn text(c: &Connection, sql: &str) -> String {
    match one(c, sql) {
        Value::Text(s) => String::from(s.as_str()),
        v => panic!("not text: {v:?}"),
    }
}
fn hex(c: &Connection, sql: &str) -> String {
    text(c, &format!("SELECT hex({sql})"))
}

#[test]
fn json_text_preserves_negative_zero_integer() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT json('-0')"), "-0");
    assert_eq!(text(&c, "SELECT json('[-0]')"), "[-0]");
    assert_eq!(text(&c, "SELECT json('{\"a\":-0}')"), r#"{"a":-0}"#);
    assert_eq!(text(&c, "SELECT json('[1, -0, 2]')"), "[1,-0,2]");
    // The REAL `-0.0` was already preserved; both coexist verbatim.
    assert_eq!(text(&c, "SELECT json('[-0.0, -0]')"), "[-0.0,-0]");
}

#[test]
fn jsonb_encodes_negative_zero_under_int_tag_distinct_from_zero() {
    let c = Connection::open_memory().unwrap();
    // `-0` → INT (0x23 = size 2, tag 3) with verbatim "-0" (2D 30) — the same
    // tag family as `0` (0x13 = size 1, tag 3, "30") and `-5` (0x23, "2D 35").
    assert_eq!(hex(&c, "jsonb('-0')"), "232D30");
    assert_eq!(hex(&c, "jsonb('0')"), "1330");
    assert_eq!(hex(&c, "jsonb('-5')"), "232D35");
    // The consequential difference: `-0` and `0` are now unequal JSONB blobs.
    assert_eq!(
        one(&c, "SELECT jsonb('-0') = jsonb('0')"),
        Value::Integer(0)
    );
    assert_eq!(
        one(&c, "SELECT jsonb('-0') = jsonb('-0')"),
        Value::Integer(1)
    );
}

#[test]
fn negative_zero_round_trips_through_jsonb() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT json(jsonb('-0'))"), "-0");
    assert_eq!(text(&c, "SELECT json(jsonb('[-0]'))"), "[-0]");
}

#[test]
fn negative_zero_extracts_as_scalar_zero() {
    let c = Connection::open_memory().unwrap();
    // The extracted SQL *value* is the integer 0, matching sqlite.
    assert_eq!(one(&c, "SELECT json_extract('-0', '$')"), Value::Integer(0));
    assert_eq!(
        one(&c, "SELECT json_extract('[-0]', '$[0]')"),
        Value::Integer(0)
    );
}

#[test]
fn ordinary_integers_still_canonicalize() {
    // Regression guard: only `-0` retains verbatim text; every other decimal
    // integer keeps its canonical rendering and its INT-tag JSONB encoding, and
    // the JSON5 hex/leading-`+` forms are unaffected.
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT json('12345')"), "12345");
    assert_eq!(text(&c, "SELECT json('-12345')"), "-12345");
    assert_eq!(text(&c, "SELECT json('0')"), "0");
    assert_eq!(hex(&c, "jsonb('-12345')"), "632D3132333435");
    // JSON5 hex renders in canonical decimal and stays under INT5 (0x44).
    assert_eq!(text(&c, "SELECT json('0x1f')"), "31");
    assert_eq!(hex(&c, "jsonb('0x1f')"), "4430783166");
    assert_eq!(text(&c, "SELECT json(jsonb('0x1f'))"), "31");
    // JSON5 leading `+` normalizes away and stays under INT (0x13).
    assert_eq!(text(&c, "SELECT json('+5')"), "5");
    assert_eq!(hex(&c, "jsonb('+5')"), "1335");
}
