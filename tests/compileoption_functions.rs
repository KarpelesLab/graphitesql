//! `sqlite_compileoption_used(X)` / `sqlite_compileoption_get(N)` — ported from
//! sqlite's `compileoptionusedFunc` / `compileoptiongetFunc`. These introspect
//! *graphite's own* compile-option list (the same source `PRAGMA compile_options`
//! reports), so behavior is checked structurally against that list rather than
//! byte-for-byte against a particular sqlite build.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn scalar(c: &Connection, sql: &str) -> Value {
    let r = c.query(sql).unwrap();
    r.rows[0][0].clone()
}

#[test]
fn compileoption_used_matches_the_option_list() {
    let c = Connection::open_memory().unwrap();
    // A feature that is genuinely built in (default build has math functions).
    assert_eq!(
        scalar(
            &c,
            "SELECT sqlite_compileoption_used('ENABLE_MATH_FUNCTIONS')"
        ),
        Value::Integer(1)
    );
    // The `SQLITE_` prefix is optional (stripped), and matching is case-insensitive.
    assert_eq!(
        scalar(
            &c,
            "SELECT sqlite_compileoption_used('SQLITE_ENABLE_MATH_FUNCTIONS')"
        ),
        Value::Integer(1)
    );
    assert_eq!(
        scalar(
            &c,
            "SELECT sqlite_compileoption_used('enable_math_functions')"
        ),
        Value::Integer(1)
    );
    // An unknown option, and a proper prefix that is not a whole option, are 0.
    assert_eq!(
        scalar(&c, "SELECT sqlite_compileoption_used('NOT_A_REAL_OPTION')"),
        Value::Integer(0)
    );
    assert_eq!(
        scalar(&c, "SELECT sqlite_compileoption_used('ENABLE_MATH')"),
        Value::Integer(0)
    );
    // Empty string is 0 (the boundary char is an id char), NULL propagates.
    assert_eq!(
        scalar(&c, "SELECT sqlite_compileoption_used('')"),
        Value::Integer(0)
    );
    assert_eq!(
        scalar(&c, "SELECT sqlite_compileoption_used(NULL)"),
        Value::Null
    );
}

#[test]
fn compileoption_used_agrees_with_compile_options_pragma() {
    let c = Connection::open_memory().unwrap();
    let opts = c.query("PRAGMA compile_options").unwrap();
    for row in &opts.rows {
        let name = match &row[0] {
            Value::Text(t) => t.to_string(),
            other => panic!("expected text, got {other:?}"),
        };
        let sql = format!("SELECT sqlite_compileoption_used('{name}')");
        assert_eq!(
            scalar(&c, &sql),
            Value::Integer(1),
            "compile_options lists {name} but sqlite_compileoption_used says no"
        );
    }
}

#[test]
fn compileoption_get_walks_the_list_then_nulls() {
    let c = Connection::open_memory().unwrap();
    let opts = c.query("PRAGMA compile_options").unwrap();
    let n = opts.rows.len();
    assert!(n > 0);
    // get(i) returns the i-th option, in the same order as PRAGMA compile_options.
    for (i, row) in opts.rows.iter().enumerate() {
        let sql = format!("SELECT sqlite_compileoption_get({i})");
        assert_eq!(scalar(&c, &sql), row[0].clone(), "get({i}) mismatch");
    }
    // Past the end, and negative, yield NULL.
    assert_eq!(
        scalar(&c, &format!("SELECT sqlite_compileoption_get({n})")),
        Value::Null
    );
    assert_eq!(
        scalar(&c, "SELECT sqlite_compileoption_get(-1)"),
        Value::Null
    );
}
