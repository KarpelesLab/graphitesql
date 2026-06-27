//! `json_each`/`json_tree` accept a JSONB **blob** document, not just JSON text:
//! a BLOB argument is SQLite's binary JSONB (decoded as one complete value, with
//! trailing bytes rejected), so `json_each(jsonb(x))` walks the same structure as
//! `json_each(x)`. The hidden `json` column still echoes the raw blob argument.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

#[test]
fn json_each_walks_a_jsonb_blob_document() {
    let c = Connection::open_memory().unwrap();
    let r = rows(
        &c,
        "SELECT key, value, type FROM json_each(jsonb('{\"a\":1,\"b\":[2,3]}'))",
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("a".into()));
    assert_eq!(r[0][1], Value::Integer(1));
    assert_eq!(r[0][2], Value::Text("integer".into()));
    assert_eq!(r[1][0], Value::Text("b".into()));
    assert_eq!(r[1][1], Value::Text("[2,3]".into()));
    assert_eq!(r[1][2], Value::Text("array".into()));
}

#[test]
fn json_each_jsonb_honours_the_path_argument() {
    let c = Connection::open_memory().unwrap();
    let r = rows(
        &c,
        "SELECT value FROM json_each(jsonb('{\"a\":[7,8]}'),'$.a')",
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Integer(7));
    assert_eq!(r[1][0], Value::Integer(8));
}

#[test]
fn json_tree_walks_a_jsonb_blob_document() {
    let c = Connection::open_memory().unwrap();
    let r = rows(&c, "SELECT fullkey, atom FROM json_tree(jsonb('[10,20]'))");
    // Root array (atom NULL) then its two scalar children.
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Text("$".into()));
    assert_eq!(r[0][1], Value::Null);
    assert_eq!(r[1][0], Value::Text("$[0]".into()));
    assert_eq!(r[1][1], Value::Integer(10));
    assert_eq!(r[2][0], Value::Text("$[1]".into()));
    assert_eq!(r[2][1], Value::Integer(20));
}

#[test]
fn hidden_json_column_is_the_raw_jsonb_blob() {
    let c = Connection::open_memory().unwrap();
    // The `json` column echoes the argument verbatim: a JSONB blob stays a blob.
    let r = rows(&c, "SELECT typeof(json) FROM json_each(jsonb('[1,2]'))");
    assert_eq!(r[0][0], Value::Text("blob".into()));
}

#[test]
fn a_bare_null_jsonb_node_yields_one_row() {
    let c = Connection::open_memory().unwrap();
    // `x'00'` is a complete JSONB `null` node — a single scalar root row.
    let r = rows(&c, "SELECT type, atom FROM json_each(x'00')");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("null".into()));
    assert_eq!(r[0][1], Value::Null);
}

#[test]
fn malformed_jsonb_blob_is_rejected() {
    let c = Connection::open_memory().unwrap();
    let err = c
        .query("SELECT * FROM json_each(x'ffff')")
        .unwrap_err()
        .to_string();
    assert!(err.contains("malformed JSON"), "got: {err}");
}
