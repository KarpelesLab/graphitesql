//! SQLite preserves a JSON string's verbatim *escaped* source body in JSON text
//! output when the source used only standard-JSON escapes (`json('"\u0041"')` →
//! `"\u0041"`, not `"A"`), storing it under the JSONB `TEXTJ` tag, while still
//! yielding the *decoded* string (`A`) as a SQL value. graphite used to decode
//! the escape on parse, losing the source form. Object *keys* carry the same
//! provenance. The `\v` escape alone still renders from the decoded value (a
//! documented sqlite internal inconsistency, deliberately not mirrored).

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

#[test]
fn escaped_object_keys_keep_provenance() {
    // Object *keys* carry the same TEXTJ/TEXT5 escape provenance as string
    // values: the verbatim escaped body survives in json() text and JSONB, while
    // path lookups and the SQL key value still use the decoded key.
    let c = Connection::open_memory().unwrap();
    // TEXTJ key (`\u0041`) is emitted verbatim; a TEXT5 key (`\x41`) converts in text.
    assert_eq!(
        text(&c, r#"SELECT json('{"\u0041":1}')"#),
        r#"{"\u0041":1}"#
    );
    assert_eq!(text(&c, r#"SELECT json('{"\x41":1}')"#), r#"{"\u0041":1}"#);
    // The key body is stored verbatim in JSONB (TEXT5 tag 9, raw body `\x41`).
    assert_eq!(
        text(&c, r#"SELECT hex(jsonb('{"\x41":1}'))"#),
        "7C495C7834311331"
    );
    // Round-trips through JSONB unchanged.
    assert_eq!(
        text(&c, r#"SELECT json(jsonb('{"\x41":1}'))"#),
        r#"{"\u0041":1}"#
    );
    // Lookups resolve by the *decoded* key; json_each's key column is decoded.
    assert_eq!(
        one(&c, r#"SELECT json_extract('{"\x41":2}','$.A')"#),
        Value::Integer(2)
    );
    assert_eq!(
        one(&c, r#"SELECT key FROM json_each('{"\x41":2}')"#),
        Value::Text("A".into())
    );
    // A bare key has no provenance and renders canonically.
    assert_eq!(text(&c, r#"SELECT json('{a:1}')"#), r#"{"a":1}"#);
}

#[test]
fn json_tree_fullkey_uses_escaped_key_source() {
    // json_tree/json_each render an escaped key from its verbatim source body,
    // always double-quoted — even when the decoded key is a bare identifier that
    // would otherwise be emitted as `.key`.
    let c = Connection::open_memory().unwrap();
    let rows = c
        .query(r#"SELECT fullkey FROM json_tree('{"\x41":{"\x42":9}}')"#)
        .unwrap()
        .rows;
    let keys: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => String::from(s.as_str()),
            v => panic!("not text: {v:?}"),
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            "$".to_string(),
            r#"$."\x41""#.to_string(),
            r#"$."\x41"."\x42""#.to_string(),
        ]
    );
}
