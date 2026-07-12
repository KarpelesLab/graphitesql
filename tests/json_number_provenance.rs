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
        Value::Text(s) => String::from(s.as_str()),
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
fn json_text_keeps_overflowing_number_text() {
    // A strict JSON number whose magnitude overflows f64 to ±infinity still
    // round-trips its verbatim text in JSON *text* output — `json('1e1000')` is
    // `1e1000`, not the `9e999` infinity literal that a *computed* infinity
    // renders as. (graphite checked `is_infinite()` before the text-preserving
    // arm, dropping the source text; now the text wins.)
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT json('1e1000')"), "1e1000");
    assert_eq!(text(&c, "SELECT json('-1e1000')"), "-1e1000");
    assert_eq!(text(&c, "SELECT json('9.9e999')"), "9.9e999");
    assert_eq!(text(&c, "SELECT json('[1e1000]')"), "[1e1000]");
    assert_eq!(text(&c, "SELECT json('{\"x\":1e1000}')"), r#"{"x":1e1000}"#);
    // A bare `9e999` in the input (which *is* the infinity literal) is preserved
    // verbatim too, since it is itself a strict number.
    assert_eq!(text(&c, "SELECT json('9e999')"), "9e999");
    // The extracted SQL value of an overflowing number is f64 infinity.
    assert_eq!(
        one(&c, "SELECT json_extract('[1e1000]', '$[0]')"),
        Value::Real(f64::INFINITY)
    );
}

#[test]
fn leading_zero_integer_is_malformed() {
    // A `0` immediately followed by a digit is an invalid JSON number — `00`,
    // `007`, `00.5`, `-01` are rejected (sqlite: `malformed JSON`), while a lone
    // `0` and a `0` followed by `.`/`e` stay valid. graphite used to silently
    // accept `00` as `0`.
    let c = Connection::open_memory().unwrap();
    for bad in ["00", "007", "000", "00.5", "-00", "-01", "[01]", "[1,00]"] {
        assert!(
            c.query(&format!("SELECT json('{bad}')")).is_err(),
            "expected {bad} to be malformed JSON"
        );
    }
    // Genuinely-valid leading-`0` forms still parse.
    assert_eq!(text(&c, "SELECT json('0')"), "0");
    assert_eq!(text(&c, "SELECT json('0.5')"), "0.5");
    assert_eq!(text(&c, "SELECT json('0e1')"), "0e1");
    assert_eq!(text(&c, "SELECT json('[0,1,2]')"), "[0,1,2]");
}

#[test]
fn json5_dot_form_keeps_its_shape_with_minimal_zero() {
    // A JSON5 leading/trailing-`.` number renders with just the `0` inserted to
    // make it valid JSON, preserving the rest of the source form — `1.e5` →
    // `1.0e5`, `.5e2` → `0.5e2`, `-.5` → `-0.5` — instead of the computed float
    // (`100000.0`). graphite rendered the float for exponent forms.
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT json('1.e5')"), "1.0e5");
    assert_eq!(text(&c, "SELECT json('.5e2')"), "0.5e2");
    assert_eq!(text(&c, "SELECT json('10.e2')"), "10.0e2");
    assert_eq!(text(&c, "SELECT json('1.E5')"), "1.0E5");
    // The fixup wins even when the magnitude overflows f64 (renders before the
    // `9e999` infinity fallback).
    assert_eq!(text(&c, "SELECT json('1.e5000')"), "1.0e5000");
    // A form already valid (digits both sides, or no dot) is verbatim.
    assert_eq!(text(&c, "SELECT json('5.')"), "5.0");
    assert_eq!(text(&c, "SELECT json('-.5')"), "-0.5");
    assert_eq!(text(&c, "SELECT json('1.5e3')"), "1.5e3");
    // Survives a JSONB round-trip (FLOAT5 stores the raw `1.e5` text).
    assert_eq!(text(&c, "SELECT json(jsonb('1.e5'))"), "1.0e5");
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
