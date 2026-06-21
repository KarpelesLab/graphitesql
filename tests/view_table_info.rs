//! `PRAGMA table_info` over a VIEW reports its columns with the types SQLite
//! infers — a direct column reference takes its origin column's declared type
//! (an untyped origin shows `BLOB`), an expression shows an empty type — and
//! resolves through subqueries and nested views. Also covers the parser keeping
//! a type's `(length[, scale])`, which SQLite preserves in table_info.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn info(c: &Connection, sql: &str) -> Vec<(String, String)> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            let name = match &r[1] {
                Value::Text(s) => s.clone(),
                v => panic!("name not text: {v:?}"),
            };
            let ty = match &r[2] {
                Value::Text(s) => s.clone(),
                v => panic!("type not text: {v:?}"),
            };
            (name, ty)
        })
        .collect()
}

#[test]
fn view_column_types_match_origin_or_are_empty() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT, b TEXT, c)").unwrap();
    c.execute("CREATE VIEW v AS SELECT a, b, c FROM t").unwrap();
    assert_eq!(
        info(&c, "PRAGMA table_info(v)"),
        [
            ("a".into(), "INT".into()),
            ("b".into(), "TEXT".into()),
            ("c".into(), "BLOB".into()), // untyped origin → BLOB
        ]
    );

    // Expressions carry no type; a renamed column keeps its origin's type.
    c.execute("CREATE VIEW e AS SELECT a+1 x, b AS bb, 5 z FROM t")
        .unwrap();
    assert_eq!(
        info(&c, "PRAGMA table_info(e)"),
        [
            ("x".into(), String::new()),
            ("bb".into(), "TEXT".into()),
            ("z".into(), String::new()),
        ]
    );
}

#[test]
fn view_types_resolve_through_subqueries_and_nested_views() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a VARCHAR(9), b INT)").unwrap();
    c.execute("CREATE VIEW v AS SELECT * FROM (SELECT a FROM t)")
        .unwrap();
    assert_eq!(
        info(&c, "PRAGMA table_info(v)"),
        [("a".into(), "VARCHAR(9)".into())]
    );

    c.execute("CREATE VIEW base AS SELECT a, b FROM t").unwrap();
    c.execute("CREATE VIEW nested AS SELECT a, b FROM base")
        .unwrap();
    assert_eq!(
        info(&c, "PRAGMA table_info(nested)"),
        [
            ("a".into(), "VARCHAR(9)".into()),
            ("b".into(), "INT".into())
        ]
    );
}

#[test]
fn declared_type_keeps_its_parameters() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a VARCHAR(10), b DECIMAL(4,2), c NUMERIC(10))")
        .unwrap();
    assert_eq!(
        info(&c, "PRAGMA table_info(t)"),
        [
            ("a".into(), "VARCHAR(10)".into()),
            ("b".into(), "DECIMAL(4,2)".into()),
            ("c".into(), "NUMERIC(10)".into()),
        ]
    );
    // The parameterised type still drives affinity (VARCHAR → TEXT).
    c.execute("INSERT INTO t VALUES (123, '5', '9')").unwrap();
    assert_eq!(
        c.query("SELECT typeof(a) FROM t").unwrap().rows[0][0],
        Value::Text("text".into())
    );
}
