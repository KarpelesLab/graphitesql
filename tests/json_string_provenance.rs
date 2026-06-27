//! SQLite preserves a JSON string's verbatim *escaped* source body in JSON text
//! output when the source used only standard-JSON escapes (`json('"\u0041"')` →
//! `"\u0041"`, not `"A"`), storing it under the JSONB `TEXTJ` tag, while still
//! yielding the *decoded* string (`A`) as a SQL value. graphite used to decode
//! the escape on parse, losing the source form. JSON5-only escapes (`\x41`,
//! `\'`, `\v`, `\0`) are still rendered from the decoded value (a documented,
//! separate gap), and escaped *object keys* are likewise not yet preserved.

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
fn json_text_keeps_standard_escape_body() {
    let c = Connection::open_memory().unwrap();
    // A `\uXXXX` escape is re-emitted verbatim, not decoded to its character.
    assert_eq!(text(&c, r#"SELECT json('"\u0041"')"#), r#""\u0041""#);
    assert_eq!(text(&c, r#"SELECT json('"a\u0041b"')"#), r#""a\u0041b""#);
    // `\n` and `\/` are standard escapes and survive verbatim too.
    assert_eq!(
        text(&c, r#"SELECT json('["\u0041","\n","\/"]')"#),
        r#"["\u0041","\n","\/"]"#
    );
    // As an object value.
    assert_eq!(
        text(&c, r#"SELECT json('{"k":"\u0041"}')"#),
        r#"{"k":"\u0041"}"#
    );
}

#[test]
fn extracted_string_is_the_decoded_value() {
    let c = Connection::open_memory().unwrap();
    // The SQL value of the string is the decoded text, regardless of provenance.
    assert_eq!(
        one(&c, r#"SELECT json_extract('"\u0041"','$')"#),
        Value::Text("A".into())
    );
    assert_eq!(
        one(&c, r#"SELECT json_extract('{"k":"a\u0041b"}','$.k')"#),
        Value::Text("aAb".into())
    );
}

#[test]
fn jsonb_stores_textj_body_verbatim() {
    let c = Connection::open_memory().unwrap();
    // `"\u0041"` -> header 0x68 (size 6, TEXTJ) + the 6-byte body.
    assert_eq!(
        text(&c, r#"SELECT hex(jsonb('"\u0041"'))"#),
        "685C7530303431"
    );
    // Round-trips through JSONB back to the same text.
    assert_eq!(text(&c, r#"SELECT json(jsonb('"\u0041"'))"#), r#""\u0041""#);
    assert_eq!(
        text(&c, r#"SELECT json(jsonb('["é","x\ty"]'))"#),
        r#"["é","x\ty"]"#
    );
}

#[test]
fn plain_and_json5_strings_render_from_value() {
    let c = Connection::open_memory().unwrap();
    // A string needing no escapes is unaffected (stored TEXT).
    assert_eq!(text(&c, r#"SELECT json('"hello"')"#), r#""hello""#);
    assert_eq!(text(&c, r#"SELECT hex(jsonb('"hello"'))"#), "5768656C6C6F");
    // A string built programmatically (from a SQL TEXT) escapes canonically.
    assert_eq!(text(&c, "SELECT json_quote('a\tb')"), r#""a\tb""#);
}

#[test]
fn json5_only_escapes_convert_on_text_render() {
    // A JSON5-only `\xHH` escape is kept verbatim in JSONB (TEXT5 tag) but
    // rewritten to `\u00HH` when rendered to json() text, matching sqlite. The
    // SQL value is the decoded character.
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, r#"SELECT json('"\x41"')"#), r#""\u0041""#);
    assert_eq!(
        text(&c, r#"SELECT json('"\x4f\x4b"')"#),
        r#""\u004f\u004b""#
    );
    // A standard escape mixed in stays verbatim; only the JSON5 one converts.
    assert_eq!(text(&c, r#"SELECT json('"a\x41\nb"')"#), r#""a\u0041\nb""#);
    // `\0` (JSON5 NUL escape) is kept under TEXT5, rendered as a space.
    assert_eq!(text(&c, r#"SELECT json('"\0"')"#), r#""\u0000""#);
    // Decoded value is the character (`\x41` -> "A", length 1).
    assert_eq!(
        one(&c, r#"SELECT json_extract('"\x41"','$')"#),
        Value::Text("A".into())
    );
}

#[test]
fn text5_jsonb_stores_body_verbatim_and_round_trips() {
    let c = Connection::open_memory().unwrap();
    // `"\x41"` -> header 0x49 (size 4, TEXT5) + verbatim body `\x41`.
    assert_eq!(text(&c, r#"SELECT hex(jsonb('"\x41"'))"#), "495C783431");
    // Round-trips JSONB -> json() text with the conversion applied.
    assert_eq!(text(&c, r#"SELECT json(jsonb('"\x41"'))"#), r#""\u0041""#);
    assert_eq!(
        text(&c, r#"SELECT json(jsonb('"a\x41\nb"'))"#),
        r#""a\u0041\nb""#
    );
}
