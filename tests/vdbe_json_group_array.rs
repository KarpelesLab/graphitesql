//! `json_group_array(x)` / `jsonb_group_array(x)` and `json_group_object(k, v)` /
//! `jsonb_group_object(k, v)` run on the VDBE aggregate path (previously deferred
//! to the tree-walker). Unlike the other collecting aggregates they *include* NULL
//! arguments (as JSON `null`) and yield `[]` / `{}` — not NULL — over an empty
//! group, so the fold keeps NULLs for these kinds and the finalizer serializes via
//! the same `value_to_json` the tree-walker uses. A JSON-subtype argument
//! (`json(x)`, `x -> …`) still defers, since its text must be spliced in unquoted.
//!
//! `query_vdbe` errors on any fallback, so a passing `query_vdbe` proves the
//! aggregate ran on the VDBE. Every case is checked against the tree-walker and,
//! when available, the real `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(g INTEGER, a TEXT, n)").unwrap();
    c.execute("INSERT INTO t VALUES(1,'x',10),(1,'y',20),(2,'z',5),(3,'q',NULL)")
        .unwrap();
    c
}

/// VDBE (no fallback) == tree-walker for `sql`, returning the shared rows.
fn both(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let v = c
        .query_vdbe(sql)
        .expect("must run on the VDBE (no fallback)");
    c.set_use_vdbe(false);
    let tw = c.query(sql).unwrap();
    c.set_use_vdbe(true);
    assert_eq!(v.rows, tw.rows, "VDBE vs tree-walker diverged on `{sql}`");
    v.rows
}

#[test]
fn json_group_array_scalar_and_grouped() {
    let c = conn();
    // Whole-table: NULL is kept as JSON `null`.
    assert_eq!(
        both(&c, "SELECT json_group_array(n) FROM t"),
        vec![vec![Value::Text("[10,20,5,null]".into())]],
    );
    // Grouped: text elements are JSON-quoted; the all-NULL group yields `[null]`.
    assert_eq!(
        both(&c, "SELECT g, json_group_array(n) FROM t GROUP BY g"),
        vec![
            vec![Value::Integer(1), Value::Text("[10,20]".into())],
            vec![Value::Integer(2), Value::Text("[5]".into())],
            vec![Value::Integer(3), Value::Text("[null]".into())],
        ],
    );
    // DISTINCT dedups (first-seen order).
    assert_eq!(
        both(&c, "SELECT json_group_array(DISTINCT g) FROM t"),
        vec![vec![Value::Text("[1,2,3]".into())]],
    );
}

#[test]
fn json_group_array_empty_group_is_bracket_pair() {
    let c = conn();
    // An empty group yields `[]`, not NULL (unlike group_concat).
    assert_eq!(
        both(&c, "SELECT json_group_array(n) FROM t WHERE 0"),
        vec![vec![Value::Text("[]".into())]],
    );
}

#[test]
fn jsonb_group_array_runs_on_vdbe() {
    let c = conn();
    // The `jsonb_` variant returns the binary JSONB encoding (a blob) — identical
    // to the tree-walker.
    let rows = both(&c, "SELECT jsonb_group_array(g) FROM t");
    assert!(matches!(rows[0][0], Value::Blob(_)));
}

#[test]
fn json_subtype_argument_defers() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x TEXT)").unwrap();
    c.execute(r#"INSERT INTO t VALUES('[1,2]'),('{"a":1}')"#)
        .unwrap();
    // A `json(x)` argument carries the JSON subtype (spliced in unquoted), which
    // the VDBE path does not model — it must defer to the tree-walker.
    let q = "SELECT json_group_array(json(x)) FROM t";
    assert!(c.query_vdbe(q).is_err(), "subtype argument must fall back");
    assert_eq!(
        c.query(q).unwrap().rows,
        vec![vec![Value::Text(r#"[[1,2],{"a":1}]"#.into())]],
    );
    // A plain-text argument runs on the VDBE (strings JSON-quoted).
    assert_eq!(
        both(&c, "SELECT json_group_array(x) FROM t"),
        vec![vec![Value::Text(r#"["[1,2]","{\"a\":1}"]"#.into())]],
    );
}

#[test]
fn json_group_object_scalar_and_grouped() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(g INTEGER, k TEXT, v)").unwrap();
    c.execute("INSERT INTO t VALUES(1,'a',10),(1,'b',20),(2,'c',5),(2,'d',NULL)")
        .unwrap();
    // A NULL value is kept as JSON `null` (the object keeps every pair).
    assert_eq!(
        both(&c, "SELECT json_group_object(k, v) FROM t"),
        vec![vec![Value::Text(
            r#"{"a":10,"b":20,"c":5,"d":null}"#.into()
        )]],
    );
    // Grouped; the g=2 group carries a NULL value.
    assert_eq!(
        both(&c, "SELECT g, json_group_object(k, v) FROM t GROUP BY g"),
        vec![
            vec![Value::Integer(1), Value::Text(r#"{"a":10,"b":20}"#.into())],
            vec![Value::Integer(2), Value::Text(r#"{"c":5,"d":null}"#.into())],
        ],
    );
    // A computed value argument works.
    assert_eq!(
        both(&c, "SELECT json_group_object(k, v + 1) FROM t WHERE g = 1"),
        vec![vec![Value::Text(r#"{"a":11,"b":21}"#.into())]],
    );
    // An empty group yields `{}`.
    assert_eq!(
        both(&c, "SELECT json_group_object(k, v) FROM t WHERE 0"),
        vec![vec![Value::Text("{}".into())]],
    );
    // The `jsonb_` variant returns a blob.
    let rows = both(&c, "SELECT jsonb_group_object(k, v) FROM t");
    assert!(matches!(rows[0][0], Value::Blob(_)));
}

#[test]
fn json_group_object_subtype_value_defers() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(k TEXT, x TEXT)").unwrap();
    c.execute(r#"INSERT INTO t VALUES('a','[1]'),('b','{"z":2}')"#)
        .unwrap();
    // A `json(x)` value argument carries the JSON subtype (spliced unquoted) — the
    // VDBE path does not model it, so the whole query defers to the tree-walker.
    let q = "SELECT json_group_object(k, json(x)) FROM t";
    assert!(c.query_vdbe(q).is_err(), "subtype value must fall back");
    assert_eq!(
        c.query(q).unwrap().rows,
        vec![vec![Value::Text(r#"{"a":[1],"b":{"z":2}}"#.into())]],
    );
}

#[test]
fn json_group_array_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE t(g INTEGER, a TEXT, n);\
                 INSERT INTO t VALUES(1,'x',10),(1,'y',20),(2,'z',5),(3,'q',NULL);";
    let c = conn();
    for q in [
        "SELECT json_group_array(n) FROM t",
        "SELECT g, json_group_array(n) FROM t GROUP BY g",
        "SELECT json_group_array(DISTINCT g) FROM t",
        "SELECT json_group_array(n) FROM t WHERE 0",
        "SELECT json_group_array(a) FROM t GROUP BY g",
    ] {
        let got: String = c
            .query_vdbe(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| match v {
                        Value::Null => String::new(),
                        Value::Integer(i) => i.to_string(),
                        Value::Text(s) => s.clone(),
                        Value::Real(x) => x.to_string(),
                        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg(format!("{setup}{q};"))
            .output()
            .unwrap();
        let want = String::from_utf8(out.stdout).unwrap();
        assert_eq!(got, want.trim_end(), "VDBE vs sqlite3 diverged on {q}");
    }
}
