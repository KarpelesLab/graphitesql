//! `ORDER BY <output-alias> COLLATE <name>` — the explicit COLLATE (or a
//! parenthesized term) must not stop the alias from resolving to the output
//! column. Matched to the sqlite3 CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn col(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|mut r| match r.remove(0) {
            Value::Text(s) => s,
            Value::Integer(i) => i.to_string(),
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

#[test]
fn order_by_alias_with_collate() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(v)").unwrap();
    c.execute("INSERT INTO t VALUES('a'),('B'),('c'),('A')")
        .unwrap();
    // The alias `x` resolves through COLLATE; NOCASE groups case-insensitively.
    assert_eq!(
        col(&c, "SELECT v AS x FROM t ORDER BY x COLLATE NOCASE"),
        vec!["a", "A", "B", "c"]
    );
    // Plain alias ordering (BINARY) is unaffected: uppercase sorts first.
    assert_eq!(
        col(&c, "SELECT v AS x FROM t ORDER BY x"),
        vec!["A", "B", "a", "c"]
    );
    // A scalar SELECT with a COLLATE'd alias no longer errors "no such column".
    assert_eq!(
        col(&c, "SELECT 'B' AS x ORDER BY x COLLATE NOCASE"),
        vec!["B"]
    );
}

#[test]
fn order_by_positional_and_paren_collate() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(v)").unwrap();
    c.execute("INSERT INTO t VALUES('a'),('B'),('c')").unwrap();
    // Positional and parenthesized terms also accept a trailing COLLATE.
    assert_eq!(
        col(&c, "SELECT v AS x FROM t ORDER BY 1 COLLATE NOCASE"),
        vec!["a", "B", "c"]
    );
    assert_eq!(
        col(&c, "SELECT v AS x FROM t ORDER BY (x) COLLATE NOCASE"),
        vec!["a", "B", "c"]
    );
}
