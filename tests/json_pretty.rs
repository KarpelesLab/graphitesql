//! `json_pretty(X [, indent])` — byte-compatible with sqlite3's formatting.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn text(c: &Connection, sql: &str) -> String {
    match &c.query(sql).unwrap().rows[0][0] {
        Value::Text(s) => String::from(s.as_str()),
        other => panic!("not text: {other:?}"),
    }
}

#[test]
fn pretty_default_four_space_indent() {
    let c = Connection::open_memory().unwrap();
    let got = text(
        &c,
        "SELECT json_pretty('{\"a\":1,\"b\":[2,3],\"c\":{\"d\":4}}')",
    );
    let want = "{\n    \"a\": 1,\n    \"b\": [\n        2,\n        3\n    ],\n    \"c\": {\n        \"d\": 4\n    }\n}";
    assert_eq!(got, want);
}

#[test]
fn pretty_custom_indent_and_compact_cases() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        text(&c, "SELECT json_pretty('{\"a\":1}', '  ')"),
        "{\n  \"a\": 1\n}"
    );
    // Empty containers and scalars stay on one line.
    assert_eq!(text(&c, "SELECT json_pretty('[]')"), "[]");
    assert_eq!(text(&c, "SELECT json_pretty('{}')"), "{}");
    assert_eq!(text(&c, "SELECT json_pretty('5')"), "5");
    assert_eq!(text(&c, "SELECT json_pretty('\"hi\"')"), "\"hi\"");
}

#[test]
fn pretty_null_and_errors() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        c.query("SELECT json_pretty(NULL)").unwrap().rows[0][0],
        Value::Null
    );
    assert!(c.query("SELECT json_pretty('{bad')").is_err());
    assert!(c.query("SELECT json_pretty()").is_err());
}
