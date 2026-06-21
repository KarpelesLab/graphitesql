//! An empty `IN ()` list is always false (and `NOT IN ()` always true), even for
//! a NULL left operand — SQLite short-circuits before NULL semantics. Matched to
//! the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn v(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows.remove(0).remove(0)
}

#[test]
fn empty_in_is_false_even_for_null() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(v(&c, "SELECT NULL IN ()"), Value::Integer(0));
    assert_eq!(v(&c, "SELECT 1 IN ()"), Value::Integer(0));
    assert_eq!(v(&c, "SELECT 'x' IN ()"), Value::Integer(0));
    assert_eq!(v(&c, "SELECT NULL NOT IN ()"), Value::Integer(1));
    assert_eq!(v(&c, "SELECT 1 NOT IN ()"), Value::Integer(1));
    // A non-empty list keeps its NULL three-valued semantics.
    assert_eq!(v(&c, "SELECT NULL IN (1,2)"), Value::Null);
    assert_eq!(v(&c, "SELECT 1 IN (NULL)"), Value::Null);
    assert_eq!(v(&c, "SELECT 1 IN (NULL,1)"), Value::Integer(1));
}
