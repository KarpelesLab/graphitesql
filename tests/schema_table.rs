//! The schema catalog is queryable as `sqlite_schema` and the historical alias
//! `sqlite_master` (a read-only 5-column rowid table at page 1), and direct DML
//! against it is rejected — matching SQLite.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)",
        "CREATE INDEX ix ON t(b)",
        "CREATE VIEW v AS SELECT a FROM t",
    ] {
        c.execute(s).unwrap();
    }
    c
}

#[test]
fn schema_catalog_is_queryable_under_both_names() {
    let c = setup();
    for name in ["sqlite_schema", "sqlite_master", "SQLITE_MASTER"] {
        let r = c
            .query(&format!("SELECT count(*) FROM {name}"))
            .unwrap_or_else(|e| panic!("{name} not queryable: {e:?}"));
        // table t, index ix, view v (+ any automatic index).
        assert!(
            matches!(r.rows[0][0], Value::Integer(n) if n >= 3),
            "{name} returned too few rows"
        );
    }
}

#[test]
fn schema_columns_and_types() {
    let c = setup();
    // The five fixed columns resolve by name, with the right storage classes.
    let r = c
        .query("SELECT type, name, tbl_name, rootpage, sql FROM sqlite_master WHERE name='t'")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Text("table".into()));
    assert_eq!(r.rows[0][1], Value::Text("t".into()));
    assert_eq!(r.rows[0][2], Value::Text("t".into()));
    assert!(matches!(r.rows[0][3], Value::Integer(_)), "rootpage int");

    // type/name filtering works as expected.
    let tables = c
        .query("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap();
    assert_eq!(tables.rows, vec![vec![Value::Text("t".into())]]);

    let view = c
        .query("SELECT type FROM sqlite_master WHERE name='v'")
        .unwrap();
    assert_eq!(view.rows[0][0], Value::Text("view".into()));
}

#[test]
fn schema_catalog_is_read_only() {
    let mut c = setup();
    assert!(c.execute("DELETE FROM sqlite_master").is_err());
    assert!(c
        .execute("INSERT INTO sqlite_schema VALUES('x','y','z',1,'w')")
        .is_err());
    assert!(c.execute("UPDATE sqlite_master SET name='z'").is_err());
    // The catalog is intact after the rejected writes.
    let r = c.query("SELECT count(*) FROM sqlite_master").unwrap();
    assert!(matches!(r.rows[0][0], Value::Integer(n) if n >= 3));
}
