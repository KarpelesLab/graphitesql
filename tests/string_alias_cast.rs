//! Two parser conveniences SQLite accepts, matched to the CLI (3.50.4):
//!  - a string literal as a column or table alias (`SELECT x 'name'`, `FROM t 'm'`);
//!  - an empty type name in `CAST(x AS)` (leaves the value unchanged).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn one(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows.remove(0).remove(0)
}

#[test]
fn string_literal_column_alias() {
    let c = Connection::open_memory().unwrap();
    // Implicit and explicit (AS) string aliases parse; the value is the expression.
    assert_eq!(one(&c, "SELECT 'a' 'b'"), Value::Text("a".into()));
    assert_eq!(one(&c, "SELECT 5 'col'"), Value::Integer(5));
    assert_eq!(one(&c, "SELECT 1 AS 'name'"), Value::Integer(1));
    // The alias names the column, so it is referenceable from an outer query.
    assert_eq!(one(&c, "SELECT c2 FROM (SELECT 1 'c2')"), Value::Integer(1));
    // The column header is the alias.
    let r = c.query("SELECT 7 'lucky'").unwrap();
    assert_eq!(r.columns, vec!["lucky".to_string()]);
}

#[test]
fn string_literal_table_alias() {
    let c = Connection::open_memory().unwrap();
    // A string literal works as a (derived) table alias, including after AS.
    assert_eq!(
        one(&c, "SELECT m.c FROM (SELECT 1 c) 'm'"),
        Value::Integer(1)
    );
    assert_eq!(
        one(&c, "SELECT x.c FROM (SELECT 1 c) AS 'x'"),
        Value::Integer(1)
    );
}

#[test]
fn empty_cast_type_is_a_noop() {
    let c = Connection::open_memory().unwrap();
    // `CAST(x AS)` with no type name leaves the value unchanged (matches sqlite).
    assert_eq!(one(&c, "SELECT CAST(1 AS)"), Value::Integer(1));
    assert_eq!(one(&c, "SELECT CAST('hi' AS)"), Value::Text("hi".into()));
    // An unknown type name still applies NUMERIC affinity ('x' -> 0, 3.7 stays).
    assert_eq!(one(&c, "SELECT CAST('x' AS BANANA)"), Value::Integer(0));
    assert_eq!(one(&c, "SELECT CAST(3.7 AS BANANA)"), Value::Real(3.7));
}
