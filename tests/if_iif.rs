//! `if(...)` as SQLite's alias for `iif(...)`, the 2-argument form, and the
//! multi-branch `iif(c1,v1,c2,v2,…[,else])` form (SQLite 3.48+) which acts like a
//! CASE expression — including its short-circuit evaluation.

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
    assert_eq!(
        val(&c, "SELECT if('x', 'yes', 'no')"),
        Value::Text("no".into())
    );
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

#[test]
fn multi_branch_form_acts_like_case() {
    let c = Connection::open_memory().unwrap();
    // (when, then) pairs; an even argument count has no ELSE, so a fall-through
    // yields NULL.
    assert_eq!(val(&c, "SELECT iif(1, 2, 3, 4)"), Value::Integer(2));
    assert_eq!(val(&c, "SELECT iif(0, 2, 3, 4)"), Value::Integer(4));
    assert_eq!(val(&c, "SELECT iif(0, 2, 0, 4)"), Value::Null);
    // An odd argument count uses the trailing argument as the ELSE.
    assert_eq!(val(&c, "SELECT iif(0, 2, 3, 4, 5)"), Value::Integer(4));
    assert_eq!(val(&c, "SELECT iif(0, 0, 0, 2, 9)"), Value::Integer(9));
    assert_eq!(val(&c, "SELECT if(0, 1, 0, 1, 42)"), Value::Integer(42));
}

#[test]
fn untaken_branches_are_not_evaluated() {
    let c = Connection::open_memory().unwrap();
    // A branch that would raise (here, integer overflow) must not be evaluated
    // when its condition is not selected — `iif` desugars to a CASE, so it
    // short-circuits like SQLite rather than eagerly evaluating every argument.
    assert_eq!(
        val(&c, "SELECT iif(1, 'a', abs(-9223372036854775808))"),
        Value::Text("a".into())
    );
    assert_eq!(
        val(
            &c,
            "SELECT iif(0, abs(-9223372036854775808), 1, 'b', 'else')"
        ),
        Value::Text("b".into())
    );
}
