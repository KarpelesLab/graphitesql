//! Track E (ROADMAP §4) — cross-database write resolution.
//!
//! A write to an attached/`temp` database swaps that database in as the active
//! `main` for the *whole* statement, so a subquery or source that reads the
//! *original* main must still resolve there (main-first), as in sqlite. This is
//! handled in two layers:
//!   - `INSERT … SELECT` / `INSERT … VALUES ((SELECT …))` are materialized in the
//!     original context before the swap (`prematerialize_insert_source`).
//!   - For everything else, unqualified name resolution (`unqualified_db`) checks
//!     the active db first, then **falls back to attached databases** — and a
//!     cross-database write has swapped the original `main` into the target's
//!     attached slot, so a correlated `UPDATE/DELETE aux.t …` subquery resolves a
//!     `main` table there. This also fixes the top-level read of a name that lives
//!     only in an attached db (`SELECT … FROM s`).
//!
//! Self-contained oracle (in-memory ATTACH, deterministic — no `sqlite3` needed).
//!
//! *Known residual (E-arch-a, rare):* if a table name exists in **both** the
//! active db and an attached one and is referenced *unqualified inside a
//! cross-database write*, graphite binds the active db whereas sqlite binds main
//! first. Realistic schemas qualify such references; not exercised here.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

/// A fresh connection with an in-memory `aux` database attached.
fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c
}

fn scalar(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}

// --- INSERT into an attached table, source reading main (prematerialized) ------

#[test]
fn insert_select_from_main_into_aux() {
    let mut c = conn();
    c.execute("CREATE TABLE m(a)").unwrap();
    c.execute("INSERT INTO m VALUES(1),(2),(3)").unwrap();
    c.execute("CREATE TABLE aux.t(a)").unwrap();
    c.execute("INSERT INTO aux.t SELECT a*10 FROM m").unwrap();
    assert_eq!(scalar(&c, "SELECT sum(a) FROM aux.t"), Value::Integer(60));
}

#[test]
fn insert_values_subquery_from_main_into_aux() {
    let mut c = conn();
    c.execute("CREATE TABLE m(v)").unwrap();
    c.execute("INSERT INTO m VALUES(42)").unwrap();
    c.execute("CREATE TABLE aux.t(v)").unwrap();
    c.execute("INSERT INTO aux.t VALUES((SELECT v FROM m))")
        .unwrap();
    assert_eq!(scalar(&c, "SELECT v FROM aux.t"), Value::Integer(42));
}

#[test]
fn plain_cross_db_crud() {
    let mut c = conn();
    c.execute("CREATE TABLE aux.t(a, b)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(1, 2)").unwrap();
    c.execute("UPDATE aux.t SET b = 9 WHERE a = 1").unwrap();
    assert_eq!(
        scalar(&c, "SELECT b FROM aux.t WHERE a = 1"),
        Value::Integer(9)
    );
    c.execute("DELETE FROM aux.t WHERE a = 1").unwrap();
    assert_eq!(scalar(&c, "SELECT count(*) FROM aux.t"), Value::Integer(0));
}

// --- UPDATE/DELETE on an attached table, subquery/source reading main ----------

#[test]
fn update_set_subquery_reads_main() {
    let mut c = conn();
    c.execute("CREATE TABLE m(k, v)").unwrap();
    c.execute("INSERT INTO m VALUES(1, 99)").unwrap();
    c.execute("CREATE TABLE aux.t(k, v)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(1, 0)").unwrap();
    c.execute("UPDATE aux.t SET v = (SELECT v FROM m WHERE m.k = aux.t.k)")
        .unwrap();
    assert_eq!(scalar(&c, "SELECT v FROM aux.t"), Value::Integer(99));
}

#[test]
fn update_where_in_subquery_reads_main() {
    let mut c = conn();
    c.execute("CREATE TABLE m(k)").unwrap();
    c.execute("INSERT INTO m VALUES(2)").unwrap();
    c.execute("CREATE TABLE aux.t(k, v)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(1, 10), (2, 20)")
        .unwrap();
    c.execute("UPDATE aux.t SET v = 0 WHERE k IN (SELECT k FROM m)")
        .unwrap();
    assert_eq!(
        c.query("SELECT k, v FROM aux.t ORDER BY k").unwrap().rows,
        vec![
            vec![Value::Integer(1), Value::Integer(10)],
            vec![Value::Integer(2), Value::Integer(0)],
        ]
    );
}

#[test]
fn update_from_main_table() {
    let mut c = conn();
    c.execute("CREATE TABLE m(k, nv)").unwrap();
    c.execute("INSERT INTO m VALUES(1, 77), (2, 88)").unwrap();
    c.execute("CREATE TABLE aux.t(k, v)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(1, 0), (2, 0)").unwrap();
    c.execute("UPDATE aux.t SET v = m.nv FROM m WHERE aux.t.k = m.k")
        .unwrap();
    assert_eq!(
        c.query("SELECT k, v FROM aux.t ORDER BY k").unwrap().rows,
        vec![
            vec![Value::Integer(1), Value::Integer(77)],
            vec![Value::Integer(2), Value::Integer(88)],
        ]
    );
}

#[test]
fn delete_where_in_subquery_reads_main() {
    let mut c = conn();
    c.execute("CREATE TABLE m(k)").unwrap();
    c.execute("INSERT INTO m VALUES(1)").unwrap();
    c.execute("CREATE TABLE aux.t(k)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(1), (2), (3)").unwrap();
    c.execute("DELETE FROM aux.t WHERE k IN (SELECT k FROM m)")
        .unwrap();
    assert_eq!(
        scalar(&c, "SELECT group_concat(k) FROM aux.t"),
        Value::Text("2,3".into())
    );
}

#[test]
fn delete_where_exists_subquery_reads_main() {
    let mut c = conn();
    c.execute("CREATE TABLE m(k)").unwrap();
    c.execute("INSERT INTO m VALUES(2)").unwrap();
    c.execute("CREATE TABLE aux.t(k)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(1), (2), (3)").unwrap();
    c.execute("DELETE FROM aux.t WHERE EXISTS(SELECT 1 FROM m WHERE m.k = aux.t.k)")
        .unwrap();
    assert_eq!(
        scalar(&c, "SELECT group_concat(k) FROM aux.t"),
        Value::Text("1,3".into())
    );
}

// --- top-level unqualified name that lives only in an attached db --------------

#[test]
fn top_level_unqualified_name_in_attached_db() {
    // sqlite resolves an unqualified table by main→temp→attached; a name living
    // only in `aux` now resolves there.
    let mut c = conn();
    c.execute("CREATE TABLE aux.s(x)").unwrap();
    c.execute("INSERT INTO aux.s VALUES(7), (8)").unwrap();
    assert_eq!(scalar(&c, "SELECT sum(x) FROM s"), Value::Integer(15));
}

#[test]
fn unqualified_name_in_main_wins_over_attached() {
    // A name in both main and the attached db binds to main (main-first).
    let mut c = conn();
    c.execute("CREATE TABLE s(v)").unwrap();
    c.execute("INSERT INTO s VALUES(100)").unwrap();
    c.execute("CREATE TABLE aux.s(v)").unwrap();
    c.execute("INSERT INTO aux.s VALUES(1)").unwrap();
    assert_eq!(scalar(&c, "SELECT v FROM s"), Value::Integer(100));
    // ...and an unqualified INSERT target that lives only in aux resolves there.
    c.execute("CREATE TABLE aux.only(v)").unwrap();
    c.execute("INSERT INTO only VALUES(5)").unwrap();
    assert_eq!(scalar(&c, "SELECT v FROM aux.only"), Value::Integer(5));
}
