//! Track A: SQLite JSON functions (`json`, `json_extract`, `json_object`, …).
//! Verified differentially against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn one(c: &Connection, sql: &str) -> String {
    render(&c.query(sql).unwrap().rows[0][0])
}

#[test]
fn basics() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        one(&c, r#"SELECT json(' { "a" : 1 , "b" :[1,2,3] } ')"#),
        r#"{"a":1,"b":[1,2,3]}"#
    );
    assert_eq!(
        one(&c, "SELECT json_array(1, 2.5, 'x', null)"),
        r#"[1,2.5,"x",null]"#
    );
    assert_eq!(
        one(&c, "SELECT json_object('a', 1, 'b', json_array(1,2))"),
        r#"{"a":1,"b":[1,2]}"#
    );
    assert_eq!(one(&c, r#"SELECT json_type('{"a":1}')"#), "object");
    assert_eq!(one(&c, "SELECT json_type('[1,2]')"), "array");
    assert_eq!(one(&c, "SELECT json_type('1')"), "integer");
    assert_eq!(one(&c, "SELECT json_type('1.5')"), "real");
    assert_eq!(one(&c, r#"SELECT json_type('"hi"')"#), "text");
    assert_eq!(one(&c, "SELECT json_type('true')"), "true");
    assert_eq!(one(&c, "SELECT json_type('null')"), "null");
    assert_eq!(one(&c, "SELECT json_array_length('[1,2,3]')"), "3");
    assert_eq!(
        one(&c, r#"SELECT json_array_length('{"a":[7,8]}','$.a')"#),
        "2"
    );
    assert_eq!(
        one(&c, r#"SELECT json_extract('{"a":{"b":42}}','$.a.b')"#),
        "42"
    );
    assert_eq!(one(&c, "SELECT json_extract('[10,20,30]','$[1]')"), "20");
    assert_eq!(
        one(&c, r#"SELECT json_extract('{"a":1,"b":2}','$.a','$.b')"#),
        "[1,2]"
    );
    assert_eq!(one(&c, r#"SELECT json_extract('{"x":"hi"}','$.x')"#), "hi");
    assert_eq!(one(&c, "SELECT json_quote('hello')"), r#""hello""#);
    assert_eq!(one(&c, "SELECT json_valid('{bad}')"), "0");
    assert_eq!(one(&c, "SELECT json_valid('[1,2]')"), "1");
    // NULL propagation.
    assert_eq!(one(&c, "SELECT json(NULL)"), "NULL");
    assert_eq!(one(&c, "SELECT json_extract(NULL,'$')"), "NULL");
    // true/false extract to integer 1/0.
    assert_eq!(one(&c, "SELECT json_extract('[true,false]','$[0]')"), "1");
    assert_eq!(one(&c, "SELECT json_extract('[true,false]','$[1]')"), "0");
    // Missing path → NULL.
    assert_eq!(one(&c, r#"SELECT json_extract('{"a":1}','$.zz')"#), "NULL");
}

#[test]
fn malformed_errs() {
    let c = Connection::open_memory().unwrap();
    assert!(c.query("SELECT json('{not json}')").is_err());
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let queries = [
        r#"SELECT json(' [ 1, 2 ,3 ] ')"#,
        r#"SELECT json('{"z":1,"a":2}')"#,
        r#"SELECT json_array(1,2,3,'four',null,2.5)"#,
        r#"SELECT json_object('id',7,'tags',json_array('a','b'))"#,
        r#"SELECT json_object('nested',json_object('k','v'))"#,
        r#"SELECT json_extract('{"a":{"b":{"c":99}}}','$.a.b.c')"#,
        r#"SELECT json_extract('[1,[2,3],4]','$[1][0]')"#,
        r#"SELECT json_extract('{"a":[5,6,7]}','$.a[2]')"#,
        r#"SELECT json_extract('{"a":1,"b":2,"c":3}','$.a','$.c')"#,
        r#"SELECT json_type('[1,2,3]','$[0]')"#,
        r#"SELECT json_array_length('[]')"#,
        r#"SELECT json_array_length('[1,2,3,4,5]')"#,
        r#"SELECT json_valid('not json')"#,
        r#"SELECT json_valid('{"ok":true}')"#,
        r#"SELECT json_quote('he said "hi"')"#,
        r#"SELECT json_extract('{"u":"café"}','$.u')"#,
        // json_quote returns a JSON-subtyped argument as-is (already JSON text),
        // and quotes a plain value.
        r#"SELECT json_quote(json('5'))"#,
        r#"SELECT json_quote(json('[1,2,3]'))"#,
        r#"SELECT json_quote(json('{"a":1}'))"#,
        r#"SELECT json_quote(json_array(1,2))"#,
        r#"SELECT json_quote(json_object('k','v'))"#,
        r#"SELECT json_quote('5')"#,
        r#"SELECT json_quote(5)"#,
        r#"SELECT json_quote(2.5)"#,
        // json_valid flags: 0x01 strict JSON, 0x02 JSON5, 0x04/0x08 JSONB blob
        // (the harness reads one column, so each case is a single expression).
        r#"SELECT json_valid('{"a":1}')"#,
        r#"SELECT json_valid('{a:1}')"#,
        r#"SELECT json_valid('{a:1}',2)"#,
        r#"SELECT json_valid('{a:1}',3)"#,
        r#"SELECT json_valid('nope',2)"#,
        r#"SELECT json_valid('{a:1}',4)"#,
        r#"SELECT json_valid(jsonb('{"a":1}'))"#,
        r#"SELECT json_valid(jsonb('{"a":1}'),8)"#,
        r#"SELECT json_valid(jsonb('{"a":1}'),1)"#,
        r#"SELECT json_valid('5',8)"#,
        r#"SELECT json_valid(jsonb('5'))"#,
        // JSON subtype propagation: the json aggregates, multi-path json_extract,
        // and the `->` operator all carry the JSON subtype, so an enclosing
        // json_quote / json_array / json_object embeds them as JSON rather than
        // re-quoting the text. (`->>` and single-path scalar extracts do not.)
        r#"SELECT json_quote(json_group_array(value)) FROM (SELECT 1 AS value UNION SELECT 2)"#,
        r#"SELECT json_quote(json_group_object('a',1))"#,
        r#"SELECT json_array(json_group_array(value)) FROM (SELECT 1 AS value UNION SELECT 2)"#,
        r#"SELECT json_object('k', json_group_array(value)) FROM (SELECT 1 AS value UNION SELECT 2)"#,
        r#"SELECT json_quote(json_extract('{"a":1,"b":2}','$.a','$.b'))"#,
        r#"SELECT json_quote('{"a":1}' -> '$.a')"#,
        r#"SELECT json_quote('{"a":"hi"}' -> '$.a')"#,
        r#"SELECT json_quote('{"a":1}' ->> '$.a')"#,
    ];
    let c = Connection::open_memory().unwrap();
    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(q)
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = one(&c, q);
        // sqlite prints NULL as empty; normalize.
        let got_norm = if got == "NULL" { String::new() } else { got };
        if got_norm != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got_norm:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} JSON queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn json_group_array_and_object() {
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "CREATE TABLE t(g, v)",
        "INSERT INTO t VALUES('a',3),('a',1),('b',2),('a',NULL),('b',5)",
    ] {
        c.execute(s).unwrap();
    }
    // json_group_array includes NULLs and preserves group order.
    let r = c
        .query("SELECT g, json_group_array(v) FROM t GROUP BY g ORDER BY g")
        .unwrap();
    assert_eq!(render(&r.rows[0][1]), "[3,1,null]");
    assert_eq!(render(&r.rows[1][1]), "[2,5]");

    // json_group_object builds an object (duplicate keys allowed, int key -> text).
    assert_eq!(
        one(&c, "SELECT json_group_object(g, v) FROM t WHERE g='a'"),
        r#"{"a":3,"a":1,"a":null}"#
    );
    assert_eq!(one(&c, "SELECT json_group_object(5, 'x')"), r#"{"5":"x"}"#);

    // Text values become JSON strings; an empty group yields '[]'.
    assert_eq!(
        one(
            &c,
            "SELECT json_group_array(x) FROM (SELECT 'a' x UNION ALL SELECT 'b')"
        ),
        r#"["a","b"]"#
    );
    assert_eq!(one(&c, "SELECT json_group_array(v) FROM t WHERE 0"), "[]");

    // An ORDER BY inside the aggregate sorts the elements.
    assert_eq!(
        one(
            &c,
            "SELECT json_group_array(v ORDER BY v) FROM t WHERE g='a'"
        ),
        "[null,1,3]"
    );
    // A json() argument is embedded as JSON, not quoted (subtype propagation).
    assert_eq!(one(&c, "SELECT json_group_array(json('[1,2]'))"), "[[1,2]]");
}

#[test]
fn arrow_operator_rejects_malformed_explicit_path() {
    // The `->` / `->>` operators evaluate an explicit `$`-rooted path argument and
    // must raise sqlite's "bad JSON path" for a malformed one (they previously
    // swallowed the error and returned NULL). A bare key/index operand is wrapped
    // into `$.key` / `$[n]` and stays valid (it may contain spaces, dots, etc.).
    let c = Connection::open_memory().unwrap();
    for q in [
        "SELECT '[1,2]' -> '$bad'",
        "SELECT '[1,2]' ->> '$bad'",
        "SELECT '{\"a\":1}' -> '$x'",
    ] {
        let e = c.query(q).unwrap_err();
        assert!(format!("{e}").contains("bad JSON path"), "{q}: {e}");
    }
    for q in [
        "SELECT '{\"a\":1}' -> 'a'",
        "SELECT '{\"a b\":1}' -> 'a b'",
        "SELECT '{\"a\":1}' -> '$.a'",
        "SELECT '[1,2]' -> 0",
        "SELECT '[1,2]' ->> 1",
        "SELECT '[1,2,3]' -> '$[#-1]'",
        "SELECT '{}' -> '$.missing'",
        "SELECT null -> '$bad'",
    ] {
        assert!(c.query(q).is_ok(), "{q} should succeed");
    }
    // A bare key is a single object label even when it contains `.` or `[` —
    // sqlite's `-> 'a.b'` is the literal key "a.b", not the nested path `$.a.b`.
    assert_eq!(
        c.query("SELECT '{\"a.b\":1}' -> 'a.b'").unwrap().rows[0][0],
        Value::Text("1".into())
    );
    assert_eq!(
        c.query("SELECT '{\"a[0]\":7}' ->> 'a[0]'").unwrap().rows[0][0],
        Value::Integer(7)
    );
    // Chained arrows each take a single key.
    assert_eq!(
        c.query("SELECT '{\"a\":{\"b\":5}}' -> 'a' ->> 'b'")
            .unwrap()
            .rows[0][0],
        Value::Integer(5)
    );
}
