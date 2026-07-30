//! `RETURNING` on a view whose modification runs through an `INSTEAD OF` trigger
//! projects the view's columns — the NEW row for INSERT/UPDATE, the OLD row for
//! DELETE — exactly as SQLite does. graphite previously errored
//! "no such table: <view>" (the RETURNING path looked the view up as a table)
//! or, for UPDATE/DELETE views, returned nothing. Byte-verified against
//! sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn view_conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(a, b);
         CREATE VIEW v AS SELECT a, b FROM t;
         CREATE TRIGGER vt INSTEAD OF INSERT ON v
           BEGIN INSERT INTO t VALUES(NEW.a, NEW.b); END;
         CREATE TRIGGER vu INSTEAD OF UPDATE ON v
           BEGIN UPDATE t SET b = NEW.b WHERE a = OLD.a; END;
         CREATE TRIGGER vd INSTEAD OF DELETE ON v
           BEGIN DELETE FROM t WHERE a = OLD.a; END;",
    )
    .unwrap();
    c
}

#[test]
fn update_view_returning_projects_new_row() {
    let mut c = view_conn();
    c.execute("INSERT INTO t VALUES(1, 2), (3, 4)").unwrap();
    let r = c
        .execute_returning(
            "UPDATE v SET b = 9 WHERE a = 1 RETURNING a, b",
            &Default::default(),
        )
        .unwrap();
    assert_eq!(r.rows, [[Value::Integer(1), Value::Integer(9)]]);
    assert_eq!(
        c.query("SELECT b FROM t WHERE a = 1").unwrap().rows[0][0],
        Value::Integer(9)
    );
}

#[test]
fn delete_view_returning_projects_old_row() {
    let mut c = view_conn();
    c.execute("INSERT INTO t VALUES(1, 2), (3, 4)").unwrap();
    let r = c
        .execute_returning("DELETE FROM v WHERE a = 1 RETURNING b", &Default::default())
        .unwrap();
    assert_eq!(r.rows, [[Value::Integer(2)]]);
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(1)
    );
}

#[test]
fn returning_projects_the_new_row() {
    let mut c = view_conn();
    let r = c
        .execute_returning(
            "INSERT INTO v VALUES(5, 6) RETURNING a, b",
            &Default::default(),
        )
        .unwrap();
    assert_eq!(r.columns, ["a", "b"]);
    assert_eq!(r.rows, [[Value::Integer(5), Value::Integer(6)]]);
    // And the row really landed in the base table via the INSTEAD OF trigger.
    assert_eq!(
        c.query("SELECT a, b FROM t").unwrap().rows,
        [[Value::Integer(5), Value::Integer(6)]]
    );
}

#[test]
fn returning_expression_and_star() {
    let mut c = view_conn();
    let r = c
        .execute_returning(
            "INSERT INTO v VALUES(2, 3) RETURNING a + b AS s",
            &Default::default(),
        )
        .unwrap();
    assert_eq!(r.rows, [[Value::Integer(5)]]);

    let r = c
        .execute_returning(
            "INSERT INTO v VALUES(7, 8) RETURNING *",
            &Default::default(),
        )
        .unwrap();
    assert_eq!(r.rows, [[Value::Integer(7), Value::Integer(8)]]);
}

#[test]
fn returning_multi_row() {
    let mut c = view_conn();
    let r = c
        .execute_returning(
            "INSERT INTO v VALUES(1, 1), (2, 2) RETURNING a",
            &Default::default(),
        )
        .unwrap();
    assert_eq!(r.rows, [[Value::Integer(1)], [Value::Integer(2)]]);
}
