//! JSON5 acceptance in the JSON parser.
//!
//! SQLite's `json()` family accepts JSON5 (a superset of RFC-8259) on input but
//! still emits strict, minified JSON. These cases mirror the `sqlite3` CLI
//! (3.50.4): JSON5 in, canonical JSON out.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn text(c: &Connection, sql: &str) -> String {
    match c.query(sql).unwrap().rows.remove(0).remove(0) {
        Value::Text(s) => s,
        other => panic!("expected text, got {other:?}"),
    }
}

fn int(c: &Connection, sql: &str) -> i64 {
    match c.query(sql).unwrap().rows.remove(0).remove(0) {
        Value::Integer(i) => i,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn unquoted_object_keys() {
    let c = Connection::open_memory().unwrap();
    // Bare identifier keys; `$` and `_` are allowed as identifier characters.
    assert_eq!(text(&c, "SELECT json('{a:1}')"), r#"{"a":1}"#);
    assert_eq!(
        text(&c, "SELECT json('{a:1, $b:2, _c:3}')"),
        r#"{"a":1,"$b":2,"_c":3}"#
    );
    // Keys re-serialize as quoted strings; nested bare keys work too.
    assert_eq!(text(&c, "SELECT json('{a:{b:5}}')"), r#"{"a":{"b":5}}"#);
    assert_eq!(int(&c, "SELECT json_extract('{a:{b:5}}','$.a.b')"), 5);
}

#[test]
fn single_quoted_strings() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT json('{''k'':''v''}')"), r#"{"k":"v"}"#);
    // `\'` is a literal quote inside a single-quoted string; a bare `'` ends it.
    assert_eq!(
        text(&c, r#"SELECT json('{x:''a\'' b''}')"#),
        r#"{"x":"a' b"}"#
    );
    // `\'` is also accepted inside a double-quoted string.
    assert_eq!(text(&c, r#"SELECT json('"a\'' b"')"#), r#""a' b""#);
}

#[test]
fn trailing_commas() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, r#"SELECT json('{"a":1,}')"#), r#"{"a":1}"#);
    assert_eq!(text(&c, "SELECT json('[1,2,]')"), "[1,2]");
    assert_eq!(
        text(&c, "SELECT json('{a:1, b:[2,3,], c:{d:4,}}')"),
        r#"{"a":1,"b":[2,3],"c":{"d":4}}"#
    );
    // Only one trailing comma; a leading or doubled comma stays invalid
    // (matching sqlite, which rejects `[,]` / `[1,,]` even under JSON5).
    assert!(c.query("SELECT json('[,]')").is_err());
    assert!(c.query("SELECT json('[1,,]')").is_err());
    assert!(c.query("SELECT json('{,}')").is_err());
}

#[test]
fn comments() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        text(&c, "SELECT json('[1, /* block */ 2, // line\n3]')"),
        "[1,2,3]"
    );
    assert_eq!(text(&c, "SELECT json('// hi\n5')"), "5");
    assert_eq!(text(&c, "SELECT json('[/* a */]')"), "[]");
    assert_eq!(text(&c, "SELECT json('{ /* */ }')"), "{}");
    // `json_error_position` reports 0 for well-formed JSON5 with comments.
    assert_eq!(int(&c, "SELECT json_error_position('/* x */ 5')"), 0);
    // An unterminated block comment is a syntax error at the document start.
    assert_eq!(int(&c, "SELECT json_error_position('/* unterminated')"), 1);
}

#[test]
fn numbers() {
    let c = Connection::open_memory().unwrap();
    // Leading `+`.
    assert_eq!(text(&c, "SELECT json('+5')"), "5");
    // Leading / trailing decimal point.
    assert_eq!(text(&c, "SELECT json('.5')"), "0.5");
    assert_eq!(text(&c, "SELECT json('5.')"), "5.0");
    assert_eq!(text(&c, "SELECT json('+.5')"), "0.5");
    assert_eq!(text(&c, "SELECT json('-.5')"), "-0.5");
    // Hexadecimal, with optional sign.
    assert_eq!(text(&c, "SELECT json('0xFF')"), "255");
    assert_eq!(text(&c, "SELECT json('0x10')"), "16");
    assert_eq!(text(&c, "SELECT json('-0x10')"), "-16");
    assert_eq!(text(&c, "SELECT json('+0x10')"), "16");
    assert_eq!(text(&c, "SELECT json_type('0x10')"), "integer");
    // Hex up to i64::MAX is exact.
    assert_eq!(
        text(&c, "SELECT json('0x7FFFFFFFFFFFFFFF')"),
        "9223372036854775807"
    );
    // A hex value is an integer SQL value (wrapping modulo 2^64), matching
    // sqlite (`0xFFFF…FFFF` extracts to -1).
    assert_eq!(
        int(&c, "SELECT json_extract('[0xFFFFFFFFFFFFFFFF]','$[0]')"),
        -1
    );
}

#[test]
fn infinity_and_nan() {
    let c = Connection::open_memory().unwrap();
    // Infinity renders as the round-tripping `±9e999`; its type is `real`.
    assert_eq!(text(&c, "SELECT json('Infinity')"), "9e999");
    assert_eq!(text(&c, "SELECT json('-Infinity')"), "-9e999");
    assert_eq!(text(&c, "SELECT json('+Infinity')"), "9e999");
    assert_eq!(text(&c, "SELECT json_type('Infinity')"), "real");
    // NaN parses to JSON `null` (type `null`).
    assert_eq!(text(&c, "SELECT json('NaN')"), "null");
    assert_eq!(text(&c, "SELECT json_type('NaN')"), "null");
    // Inside containers, Infinity/NaN render as `9e999` / `null`.
    assert_eq!(
        text(&c, "SELECT json('[Infinity, NaN, -Infinity]')"),
        "[9e999,null,-9e999]"
    );
    assert_eq!(
        text(&c, "SELECT json('{a:Infinity, b:NaN}')"),
        r#"{"a":9e999,"b":null}"#
    );
}

#[test]
fn string_escapes() {
    let c = Connection::open_memory().unwrap();
    // JSON5 line continuation: a backslash before a newline is dropped.
    assert_eq!(text(&c, "SELECT json('\"a\\\nb\"')"), r#""ab""#);
    // `\xHH` hex escape and `\0` produce the expected bytes (values match
    // sqlite; the canonical re-escaping of these JSON5-only escapes is a known
    // divergence — see the module notes).
    assert_eq!(
        int(&c, r#"SELECT unicode(json_extract('{"x":"\x41"}','$.x'))"#),
        65 // 'A'
    );
    // The `\0` escape yields a single NUL byte: it is stored (hex `00`), but
    // `length()` counts characters up to the first NUL, so it reports 0 — both
    // matching the sqlite3 CLI.
    assert_eq!(
        text(&c, r#"SELECT hex(json_extract('{"x":"\0"}','$.x'))"#),
        "00"
    );
    assert_eq!(
        int(&c, r#"SELECT length(json_extract('{"x":"\0"}','$.x'))"#),
        0 // no characters precede the single NUL
    );
}

#[test]
fn json_valid_is_strict_rfc8259() {
    let c = Connection::open_memory().unwrap();
    // The 1-argument `json_valid` is restricted to strict RFC-8259 JSON, matching
    // sqlite (its JSON5 acceptance is behind the 2-arg flag form). So although
    // `json()`/`json_extract()` accept JSON5, `json_valid` rejects JSON5-only
    // forms — verified byte-for-byte against the sqlite3 CLI.
    assert_eq!(int(&c, "SELECT json_valid('{a:1}')"), 0); // unquoted key
    assert_eq!(int(&c, r#"SELECT json_valid('{"a":1,}')"#), 0); // trailing comma
    assert_eq!(int(&c, "SELECT json_valid('[1,2,]')"), 0); // trailing comma
    assert_eq!(int(&c, "SELECT json_valid('\"x\"')"), 1); // strict scalar
    // Strict JSON is valid; genuine garbage is invalid.
    assert_eq!(int(&c, r#"SELECT json_valid('{"a":1}')"#), 1);
    assert_eq!(int(&c, "SELECT json_valid('[1,2]')"), 1);
    assert_eq!(int(&c, "SELECT json_valid('5')"), 1);
    assert_eq!(int(&c, "SELECT json_valid('1.5e3')"), 1);
    assert_eq!(int(&c, "SELECT json_valid('{bad}')"), 0);
    assert_eq!(int(&c, "SELECT json_valid('nul')"), 0);
}

#[test]
fn strict_json_unchanged() {
    let c = Connection::open_memory().unwrap();
    // Strict JSON keeps round-tripping, output unchanged.
    assert_eq!(text(&c, r#"SELECT json('  {"a":1} ')"#), r#"{"a":1}"#);
    assert_eq!(text(&c, "SELECT json(' [ 1, 2 ,3 ] ')"), "[1,2,3]");
    assert_eq!(
        text(&c, r#"SELECT json('{"z":1,"a":2}')"#),
        r#"{"z":1,"a":2}"#
    );
    assert_eq!(text(&c, r#"SELECT json('"hello"')"#), r#""hello""#);
}
