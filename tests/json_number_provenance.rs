//! SQLite preserves a JSON number's verbatim source text in JSON *text* output
//! (`json('1e2')` → `1e2`, `json('1.50')` → `1.50`, `json('-0.0')` → `-0.0`)
//! while still yielding the canonical `f64` as a SQL value (`json_extract` of a
//! number is `100.0`). graphite used to canonicalize the text on parse, losing
//! it. This also makes `jsonb()` of such numbers byte-match sqlite.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn one(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}
fn text(c: &Connection, sql: &str) -> String {
    match one(c, sql) {
        Value::Text(s) => s,
        v => panic!("not text: {v:?}"),
    }
}

#[test]
fn json_text_keeps_the_source_number_form() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT json('1e2')"), "1e2");
    assert_eq!(text(&c, "SELECT json('1E10')"), "1E10");
    assert_eq!(text(&c, "SELECT json('-0.0')"), "-0.0");
    assert_eq!(
        text(&c, "SELECT json('[1e2, 1.50, 100, 2.0, 1.5e3]')"),
        "[1e2,1.50,100,2.0,1.5e3]"
    );
    assert_eq!(
        text(&c, "SELECT json('{\"a\":1e2,\"b\":2.50}')"),
        r#"{"a":1e2,"b":2.50}"#
    );
    // A number built programmatically (from a SQL REAL) still renders canonically.
    assert_eq!(text(&c, "SELECT json_array(1.5, 2.0)"), "[1.5,2.0]");
}

#[test]
fn extracted_scalar_is_the_canonical_value() {
    let c = Connection::open_memory().unwrap();
    // The SQL value of a JSON number is the f64, not its text.
    assert_eq!(
        one(&c, "SELECT json_extract('[1e2]', '$[0]')"),
        Value::Real(100.0)
    );
    assert_eq!(
        one(&c, "SELECT json_extract('{\"a\":1.50}', '$.a')"),
        Value::Real(1.5)
    );
}

#[test]
fn jsonb_round_trip_preserves_number_text() {
    let c = Connection::open_memory().unwrap();
    // The FLOAT payload carries the source text, so json(jsonb(x)) keeps it.
    assert_eq!(text(&c, "SELECT json(jsonb('[1e2,2.50]'))"), "[1e2,2.50]");
    // jsonb('1e10') now encodes "1e10" (FLOAT, size 4) — byte-matching sqlite.
    assert_eq!(text(&c, "SELECT hex(jsonb('1e10'))"), "4531653130");
}
