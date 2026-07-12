//! SQLite's JSONB (binary JSON) family: `jsonb()` and the `jsonb_*` functions
//! produce the binary encoding, and every JSON function accepts a JSONB blob as
//! its document. The byte encoding and round-trips are verified against the
//! sqlite3 3.50.4 CLI (see the differential corpus); these tests pin the
//! library-level behaviour.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn one(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}

fn hex(c: &Connection, sql: &str) -> String {
    match one(c, &format!("SELECT hex({sql})")) {
        Value::Text(s) => String::from(s.as_str()),
        v => panic!("hex not text: {v:?}"),
    }
}

#[test]
fn jsonb_encoding_matches_sqlite_bytes() {
    let c = Connection::open_memory().unwrap();
    // Header nibbles: low = type, high = size. INT/strings store ASCII/raw bytes.
    assert_eq!(hex(&c, "jsonb('null')"), "00");
    assert_eq!(hex(&c, "jsonb('42')"), "233432"); // INT, size 2, "42"
    assert_eq!(hex(&c, "jsonb('\"hi\"')"), "276869"); // TEXT, size 2, "hi"
    assert_eq!(hex(&c, "jsonb('[1,2,3]')"), "6B133113321333");
    assert_eq!(hex(&c, "jsonb('{\"a\":1}')"), "4C17611331");
    // jsonb_array equals jsonb of the equivalent literal.
    assert_eq!(hex(&c, "jsonb_array(1,2,3)"), hex(&c, "jsonb('[1,2,3]')"));
}

#[test]
fn json_functions_accept_jsonb_input() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        one(&c, "SELECT json(jsonb('{\"a\":1,\"b\":[2,3]}'))"),
        Value::Text(r#"{"a":1,"b":[2,3]}"#.into())
    );
    assert_eq!(
        one(
            &c,
            "SELECT json_extract(jsonb('{\"a\":10,\"b\":[20,30]}'), '$.b[1]')"
        ),
        Value::Integer(30)
    );
    assert_eq!(
        one(&c, "SELECT json_type(jsonb('[1,2,3]'))"),
        Value::Text("array".into())
    );
    assert_eq!(
        one(&c, "SELECT json_array_length(jsonb('[1,2,3,4]'))"),
        Value::Integer(4)
    );
}

#[test]
fn jsonb_functions_return_blobs_and_compose() {
    let c = Connection::open_memory().unwrap();
    assert!(matches!(one(&c, "SELECT jsonb('[1]')"), Value::Blob(_)));
    // A jsonb_extract of a container is itself a JSONB blob.
    assert!(matches!(
        one(&c, "SELECT jsonb_extract(jsonb('{\"a\":[1,2]}'), '$.a')"),
        Value::Blob(_)
    ));
    // ...but a scalar extract is the SQL value.
    assert_eq!(
        one(&c, "SELECT jsonb_extract(jsonb('{\"a\":5}'), '$.a')"),
        Value::Integer(5)
    );
    // jsonb_* results compose as values into other jsonb constructors/mutators.
    assert_eq!(
        one(
            &c,
            "SELECT json(jsonb_object('a', 1, 'b', jsonb_array(2, 3)))"
        ),
        Value::Text(r#"{"a":1,"b":[2,3]}"#.into())
    );
    assert_eq!(
        one(
            &c,
            "SELECT json_extract(jsonb_set('{\"a\":1}', '$.b', jsonb('[7,8]')), '$.b[1]')"
        ),
        Value::Integer(8)
    );
    // A non-JSONB blob is still rejected as a JSON value.
    assert!(c.query("SELECT json_array(x'4142')").is_err());
}

#[test]
fn jsonb_mutators_and_aggregates() {
    let mut c = Connection::open_memory().unwrap();
    assert_eq!(
        one(&c, "SELECT json(jsonb_remove('{\"a\":1,\"b\":2}', '$.a'))"),
        Value::Text(r#"{"b":2}"#.into())
    );
    assert_eq!(
        one(&c, "SELECT json(jsonb_patch('{\"a\":1}', '{\"b\":2}'))"),
        Value::Text(r#"{"a":1,"b":2}"#.into())
    );
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES (1),(2),(3)").unwrap();
    assert_eq!(
        one(&c, "SELECT json(jsonb_group_array(x)) FROM t"),
        Value::Text("[1,2,3]".into())
    );
    assert!(matches!(
        one(&c, "SELECT jsonb_group_array(x) FROM t"),
        Value::Blob(_)
    ));
}

#[test]
fn json5_number_forms_use_int5_float5_tags() {
    // JSON5-only number forms keep their verbatim text under the INT5/FLOAT5 tags
    // (matching sqlite byte-for-byte), while `json()` still renders them
    // canonically. A leading `+` is normalized to a strict INT/FLOAT.
    let c = Connection::open_memory().unwrap();

    // `.5` and `5.` → FLOAT5 (type nibble 6) with the raw text.
    assert_eq!(hex(&c, "jsonb('.5')"), "262E35"); // size 2, type 6, ".5"
    assert_eq!(hex(&c, "jsonb('5.')"), "26352E"); // ".", raw "5."
    assert_eq!(hex(&c, "jsonb('-.5')"), "362D2E35"); // size 3, type 6, "-.5"

    // Hex integers → INT5 (type nibble 4) with the raw `0x…` text.
    assert_eq!(hex(&c, "jsonb('0xFF')"), "4430784646"); // size 4, type 4, "0xFF"
    assert_eq!(hex(&c, "jsonb('0x10')"), "4430783130");

    // Strict numbers keep the FLOAT (type 5) tag and verbatim text.
    assert_eq!(hex(&c, "jsonb('1.5e3')"), "55312E356533");
    assert_eq!(hex(&c, "jsonb('1.50')"), "45312E3530");

    // A leading `+` normalizes (no `*5` tag): `+5` → INT "5", `+0.5` → FLOAT "0.5".
    assert_eq!(hex(&c, "jsonb('+5')"), "1335"); // size 1, type 3 (INT), "5"
    assert_eq!(hex(&c, "jsonb('+0.5')"), "35302E35"); // type 5 (FLOAT), "0.5"

    // `json()` of these renders canonically.
    assert_eq!(
        one(&c, "SELECT json(jsonb('.5'))"),
        Value::Text("0.5".into())
    );
    assert_eq!(
        one(&c, "SELECT json(jsonb('0xFF'))"),
        Value::Text("255".into())
    );
    assert_eq!(
        one(&c, "SELECT json(jsonb('5.'))"),
        Value::Text("5.0".into())
    );
    // ...but a strict form keeps its verbatim text.
    assert_eq!(
        one(&c, "SELECT json(jsonb('1.50'))"),
        Value::Text("1.50".into())
    );
}
