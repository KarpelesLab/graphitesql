//! `if(...)` as SQLite's alias for `iif(...)`, and the 2-argument form.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn val(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}

#[test]
fn if_is_alias_for_iif() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(val(&c, "SELECT if(1, 2, 3)"), Value::Integer(2));
    assert_eq!(val(&c, "SELECT if(0, 2, 3)"), Value::Integer(3));
    // NULL condition is not true → the else branch.
    assert_eq!(val(&c, "SELECT if(NULL, 2, 3)"), Value::Integer(3));
    assert_eq!(val(&c, "SELECT if('x', 'yes', 'no')"), Value::Text("no".into()));
}

#[test]
fn two_argument_form_yields_null_when_false() {
    let c = Connection::open_memory().unwrap();
    // Both spellings: the 2-arg form returns NULL when the condition isn't true.
    assert_eq!(val(&c, "SELECT if(1, 'a')"), Value::Text("a".into()));
    assert_eq!(val(&c, "SELECT if(0, 'a')"), Value::Null);
    assert_eq!(val(&c, "SELECT iif(1, 'a')"), Value::Text("a".into()));
    assert_eq!(val(&c, "SELECT iif(0, 'a')"), Value::Null);
}

#[test]
fn wrong_arity_is_an_error() {
    let c = Connection::open_memory().unwrap();
    assert!(c.query("SELECT if(1)").is_err());
    assert!(c.query("SELECT iif()").is_err());
}
