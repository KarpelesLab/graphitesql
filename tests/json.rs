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
