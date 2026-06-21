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
        Value::Text(s) => s,
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
