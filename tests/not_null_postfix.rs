//! The postfix `expr NOT NULL` operator (the two-word spelling of `IS NOT
//! NULL`, alongside the existing `NOTNULL`). Matched to the sqlite3 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn row(c: &Connection, sql: &str) -> Vec<Value> {
    c.query(sql).unwrap().rows.remove(0)
}

#[test]
fn not_null_postfix() {
    let c = Connection::open_memory().unwrap();
    // `x NOT NULL` == `x IS NOT NULL`.
    assert_eq!(
        row(&c, "SELECT NULL NOT NULL, 1 NOT NULL, 'x' NOT NULL"),
        vec![Value::Integer(0), Value::Integer(1), Value::Integer(1)]
    );
    // Agrees with the one-word NOTNULL and the IS NOT NULL forms.
    assert_eq!(
        row(&c, "SELECT 5 NOTNULL, 5 NOT NULL, 5 IS NOT NULL"),
        vec![Value::Integer(1), Value::Integer(1), Value::Integer(1)]
    );
    assert_eq!(
        row(&c, "SELECT NULL NOTNULL, NULL NOT NULL, NULL IS NOT NULL"),
        vec![Value::Integer(0), Value::Integer(0), Value::Integer(0)]
    );
    // Usable in WHERE.
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE TABLE t(a)").unwrap();
    conn.execute("INSERT INTO t VALUES(1),(NULL),(3)").unwrap();
    assert_eq!(
        conn.query("SELECT count(*) FROM t WHERE a NOT NULL")
            .unwrap()
            .rows[0][0],
        Value::Integer(2)
    );
}

#[test]
fn not_prefixed_operators_unaffected() {
    let c = Connection::open_memory().unwrap();
    for (sql, want) in [
        ("SELECT 3 NOT IN (1,2)", 1),
        ("SELECT 'a' NOT LIKE 'b'", 1),
        ("SELECT 'a' NOT GLOB 'b'", 1),
        ("SELECT 5 NOT BETWEEN 1 AND 3", 1),
    ] {
        assert_eq!(row(&c, sql), vec![Value::Integer(want)]);
    }
    // Prefix NOT still works: NOT NULL (prefix) is NULL.
    assert_eq!(row(&c, "SELECT NOT NULL"), vec![Value::Null]);
}
