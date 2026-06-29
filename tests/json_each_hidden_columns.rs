//! `json_each`/`json_tree` expose two *hidden* input columns, `json` and `root`,
//! that echo the function's arguments: `json` is the document (constant on every
//! row), `root` is the path (default `$`). Like SQLite's, they are resolvable by
//! name but excluded from `*` / `tbl.*` expansion, so `SELECT *` still yields only
//! the eight visible columns.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}
fn one(c: &Connection, sql: &str) -> Value {
    rows(c, sql)[0][0].clone()
}

#[test]
fn star_excludes_the_hidden_json_and_root_columns() {
    let c = Connection::open_memory().unwrap();
    // The visible schema is exactly eight columns; `json`/`root` are hidden.
    let r = rows(&c, "SELECT * FROM json_each('[7,8]')");
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].len(), 8);
    // `tbl.*` is hidden-aware too.
    let r = rows(&c, "SELECT je.* FROM json_each('[7]') je");
    assert_eq!(r[0].len(), 8);
    // A join of two walks concatenates only the visible columns (8 + 8).
    let r = rows(&c, "SELECT * FROM json_each('[1]') a, json_each('[2]') b");
    assert_eq!(r[0].len(), 16);
}

#[test]
fn json_column_echoes_the_document_argument() {
    let c = Connection::open_memory().unwrap();
    // Constant on every row, verbatim text of the argument.
    let r = rows(&c, "SELECT json FROM json_each('[1,2]')");
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("[1,2]".into()));
    assert_eq!(r[1][0], Value::Text("[1,2]".into()));
    // A scalar document echoes as given.
    assert_eq!(
        one(&c, "SELECT json FROM json_each('5')"),
        Value::Text("5".into())
    );
    // json_tree exposes it the same way.
    assert_eq!(
        one(&c, "SELECT json FROM json_tree('{\"a\":1}')"),
        Value::Text("{\"a\":1}".into())
    );
}

#[test]
fn root_column_is_the_path_argument() {
    let c = Connection::open_memory().unwrap();
    // Defaults to `$` when no path is given.
    assert_eq!(
        one(&c, "SELECT root FROM json_each('[1,2]')"),
        Value::Text("$".into())
    );
    // Echoes an explicit path argument.
    assert_eq!(
        one(&c, "SELECT root FROM json_each('{\"a\":9}','$.a')"),
        Value::Text("$.a".into())
    );
}

#[test]
fn hidden_columns_are_usable_in_where_and_explicit_select() {
    let c = Connection::open_memory().unwrap();
    // Resolvable by name in the result list and the WHERE clause.
    let r = rows(
        &c,
        "SELECT value, json, root FROM json_each('[1,2]') WHERE json='[1,2]'",
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][1], Value::Text("[1,2]".into()));
    assert_eq!(r[0][2], Value::Text("$".into()));
    // Filtering on the (constant) root column keeps every row.
    let r = rows(&c, "SELECT value FROM json_each('[1,2]') WHERE root='$'");
    assert_eq!(r.len(), 2);
}

#[test]
fn bare_form_is_driven_by_a_where_constraint_on_json() {
    // `json_each` / `json_tree` written WITHOUT parentheses are eponymous virtual
    // tables: the document comes from an equality constraint on the hidden `json`
    // column (and an optional `root`), exactly like SQLite.
    let c = Connection::open_memory().unwrap();
    let vals: Vec<Value> = rows(&c, "SELECT value FROM json_each WHERE json='[10,20,30]'")
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(
        vals,
        vec![Value::Integer(10), Value::Integer(20), Value::Integer(30)]
    );

    // An optional `root` constraint walks a sub-document.
    let vals: Vec<Value> = rows(
        &c,
        "SELECT value FROM json_each WHERE json='{\"a\":[7,8]}' AND root='$.a'",
    )
    .into_iter()
    .map(|r| r[0].clone())
    .collect();
    assert_eq!(vals, vec![Value::Integer(7), Value::Integer(8)]);

    // json_tree bare works the same way (here: the array node then its elements).
    assert_eq!(
        rows(&c, "SELECT value FROM json_tree WHERE json='[1,2]'").len(),
        3
    );

    // The hidden columns still echo and are excluded from `*`.
    assert_eq!(
        one(&c, "SELECT json FROM json_each WHERE json='[1,2]'"),
        Value::Text("[1,2]".into())
    );
    assert_eq!(
        rows(&c, "SELECT * FROM json_each WHERE json='[1,2]'")[0].len(),
        8
    );

    // No constraint, or a non-equality, drives nothing → no rows (not an error),
    // like a bare argument-less reference.
    assert!(rows(&c, "SELECT value FROM json_each").is_empty());
    assert!(rows(&c, "SELECT value FROM json_each WHERE json LIKE '[1]'").is_empty());
}
