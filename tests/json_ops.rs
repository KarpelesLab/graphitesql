//! Track A: JSON `->`/`->>` operators and the `json_set`/`json_insert`/
//! `json_replace`/`json_remove`/`json_patch` mutators. Verified against sqlite3.

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
fn operators() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(one(&c, r#"SELECT '{"a":1,"b":"x"}' -> '$.a'"#), "1");
    assert_eq!(one(&c, r#"SELECT '{"a":1,"b":"x"}' -> '$.b'"#), r#""x""#);
    assert_eq!(one(&c, r#"SELECT '{"a":1,"b":"x"}' ->> '$.b'"#), "x");
    assert_eq!(one(&c, r#"SELECT '{"c":[1,2]}' -> '$.c'"#), "[1,2]");
    assert_eq!(one(&c, r#"SELECT '{"a":1}' -> 'a'"#), "1"); // bare label
    assert_eq!(one(&c, "SELECT '[7,8,9]' -> 2"), "9"); // integer index
    assert_eq!(one(&c, "SELECT '[7,8,9]' ->> 2"), "9");
    assert_eq!(one(&c, r#"SELECT '{"a":1}' -> '$.zz'"#), "NULL");
    assert_eq!(one(&c, r#"SELECT '{"a":1}' ->> '$.zz'"#), "NULL");
}

#[test]
fn mutators() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        one(&c, r#"SELECT json_set('{"a":1}','$.b',9)"#),
        r#"{"a":1,"b":9}"#
    );
    assert_eq!(
        one(&c, r#"SELECT json_set('{"a":1}','$.a',9)"#),
        r#"{"a":9}"#
    );
    assert_eq!(
        one(&c, r#"SELECT json_insert('{"a":1}','$.a',9)"#),
        r#"{"a":1}"#
    );
    assert_eq!(
        one(&c, r#"SELECT json_insert('{"a":1}','$.b',9)"#),
        r#"{"a":1,"b":9}"#
    );
    assert_eq!(
        one(&c, r#"SELECT json_replace('{"a":1}','$.b',9)"#),
        r#"{"a":1}"#
    );
    assert_eq!(
        one(&c, r#"SELECT json_replace('{"a":1}','$.a',9)"#),
        r#"{"a":9}"#
    );
    assert_eq!(
        one(&c, r#"SELECT json_remove('{"a":1,"b":2}','$.a')"#),
        r#"{"b":2}"#
    );
    assert_eq!(
        one(
            &c,
            r#"SELECT json_patch('{"a":1,"b":2}','{"b":null,"c":3}')"#
        ),
        r#"{"a":1,"c":3}"#
    );
    // Nested construction embeds as JSON.
    assert_eq!(
        one(&c, r#"SELECT json_set('{}','$.arr',json_array(1,2))"#),
        r#"{"arr":[1,2]}"#
    );
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let queries = [
        r#"SELECT '{"a":{"b":5}}' -> '$.a'"#,
        r#"SELECT '{"a":{"b":5}}' -> '$.a.b'"#,
        r#"SELECT '{"a":{"b":5}}' ->> '$.a.b'"#,
        r#"SELECT '{"x":"hi"}' -> '$.x'"#,
        r#"SELECT '{"x":"hi"}' ->> '$.x'"#,
        r#"SELECT '[10,20,30]' -> 1"#,
        r#"SELECT '[10,20,30]' ->> '$[2]'"#,
        r#"SELECT json_set('{"a":1,"b":2}','$.a',10,'$.c',30)"#,
        r#"SELECT json_insert('{"a":1}','$.a',2,'$.b',3)"#,
        r#"SELECT json_replace('{"a":1,"b":2}','$.b',20,'$.z',99)"#,
        r#"SELECT json_remove('{"a":1,"b":2,"c":3}','$.b')"#,
        r#"SELECT json_remove('[1,2,3,4]','$[1]')"#,
        r#"SELECT json_patch('{"a":{"x":1}}','{"a":{"y":2}}')"#,
        r#"SELECT json_patch('{"a":1}','{"a":{"nested":true}}')"#,
        r#"SELECT json_set('{}','$.o',json_object('k','v'))"#,
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
        let got = if got == "NULL" { String::new() } else { got };
        if got != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} JSON-op queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
