//! A CHECK-constraint violation names the constraint like sqlite: its name when
//! written `CONSTRAINT <name> CHECK …`, else the verbatim expression text
//! (`CHECK constraint failed: <label>`). Also covers the parsing fix that a
//! `CONSTRAINT <name>` prefix is not mistaken for a column type.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn violation(setup: &str, failing: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(setup).unwrap();
    c.execute(failing).unwrap_err().to_string()
}

#[test]
fn unnamed_check_reports_verbatim_expression() {
    // The expression's source text is preserved exactly, spacing and all.
    assert!(
        violation("CREATE TABLE t(a, CHECK(a>0))", "INSERT INTO t VALUES (-1)")
            .contains("CHECK constraint failed: a>0")
    );
    assert!(violation(
        "CREATE TABLE t(a, CHECK(a > 0))",
        "INSERT INTO t VALUES (-1)"
    )
    .contains("CHECK constraint failed: a > 0"));
    // A column-level CHECK, and the *failing* one of several.
    assert!(violation(
        "CREATE TABLE t(a CHECK(a<>5), b)",
        "INSERT INTO t VALUES (5, 0)"
    )
    .contains("CHECK constraint failed: a<>5"));
    assert!(violation(
        "CREATE TABLE t(a, b, CHECK(a>0), CHECK(b>a))",
        "INSERT INTO t VALUES (5, 1)"
    )
    .contains("CHECK constraint failed: b>a"));
}

#[test]
fn named_check_reports_the_constraint_name() {
    assert!(violation(
        "CREATE TABLE t(a, CONSTRAINT ck CHECK(a>0))",
        "INSERT INTO t VALUES (-1)",
    )
    .contains("CHECK constraint failed: ck"));
    // A named *column* constraint, too.
    assert!(violation(
        "CREATE TABLE t(a CONSTRAINT col_ck CHECK(a>0))",
        "INSERT INTO t VALUES (-1)",
    )
    .contains("CHECK constraint failed: col_ck"));
}

#[test]
fn named_column_constraint_is_not_a_type() {
    // `a CONSTRAINT ck CHECK(...)` — the column has no type, exactly like sqlite
    // (previously the parser captured "CONSTRAINT ck" as the column type).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a CONSTRAINT ck CHECK(a>0))")
        .unwrap();
    let r = c.query("PRAGMA table_info(t)").unwrap();
    assert_eq!(r.rows[0][1], Value::Text("a".into()));
    assert_eq!(r.rows[0][2], Value::Text(String::new())); // empty type
}
