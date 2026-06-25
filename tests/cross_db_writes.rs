//! Track E (ROADMAP §4) — cross-database write resolution: the regression oracle.
//!
//! A write to an attached/`temp` database swaps that database in as the active
//! `main` for the *whole* statement, so a subquery or source that reads the
//! *original* main must still resolve there (main-first), as in sqlite. The
//! INSERT forms are fixed (materialized in the original context before the swap —
//! see `attach.rs`); the UPDATE/DELETE-with-subquery forms and the top-level
//! "unqualified name lives only in an attached db" read are the remaining
//! architectural work (**E1–E3**, **E-arch-a/b**).
//!
//! This is the **E0** prerequisite: a self-contained oracle (in-memory ATTACH, so
//! results are deterministic — no `sqlite3` needed) pinning the *target* behavior.
//! The cases that already work are asserted live; the ones that need the refactor
//! are `#[ignore]`d and assert the sqlite-matching result, so removing the
//! `#[ignore]` is the acceptance check when E-arch lands.

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

// ---------------------------------------------------------------------------
// Already correct (asserted live) — these guard the shipped INSERT fixes and the
// plain cross-database CRUD path.
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Track E targets — currently broken (resolve in the swapped target db instead of
// main, erroring "no such table"). `#[ignore]`d; removing the ignore is the
// acceptance check for E-arch-a/b.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Track E (E2): UPDATE SET=(subquery reading main) into an attached db"]
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
#[ignore = "Track E (E2): UPDATE WHERE IN (subquery reading main) into an attached db"]
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
#[ignore = "Track E (E1): UPDATE … FROM a main table into an attached db"]
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
#[ignore = "Track E (E3): DELETE WHERE IN (subquery reading main) from an attached db"]
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
#[ignore = "Track E (E3): DELETE WHERE EXISTS (subquery reading main) from an attached db"]
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

#[test]
#[ignore = "Track E (E-arch-a): unqualified name that lives only in an attached db"]
fn top_level_unqualified_name_in_attached_db() {
    // sqlite resolves an unqualified table by main→temp→attached; graphite only
    // searches main/temp today, so a name living only in `aux` errors.
    let mut c = conn();
    c.execute("CREATE TABLE aux.s(x)").unwrap();
    c.execute("INSERT INTO aux.s VALUES(7), (8)").unwrap();
    assert_eq!(scalar(&c, "SELECT sum(x) FROM s"), Value::Integer(15));
}
