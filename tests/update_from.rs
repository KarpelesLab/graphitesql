//! `UPDATE … SET … FROM <sources> WHERE …` (SQLite's UPDATE-FROM extension):
//! the target is joined to the FROM tables, and each matched target row is
//! updated using values from the joined row. Matched against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn conn(setup: &[&str]) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for s in setup {
        c.execute(s).unwrap();
    }
    c
}

fn pairs(c: &Connection, sql: &str) -> Vec<(i64, i64)> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Integer(a), Value::Integer(b)) => (*a, *b),
            _ => panic!("non-int row"),
        })
        .collect()
}

#[test]
fn basic_update_from_join() {
    let mut c = conn(&[
        "CREATE TABLE t(id INT, v INT)",
        "INSERT INTO t VALUES(1,10),(2,20),(3,30)",
        "CREATE TABLE s(id INT, nv INT)",
        "INSERT INTO s VALUES(1,100),(2,200)",
    ]);
    // Returns the number of updated rows; id=3 has no match and is untouched.
    assert_eq!(
        c.execute("UPDATE t SET v = s.nv FROM s WHERE t.id = s.id")
            .unwrap(),
        2
    );
    assert_eq!(
        pairs(&c, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 100), (2, 200), (3, 30)]
    );
}

#[test]
fn set_expression_reads_both_sides() {
    let mut c = conn(&[
        "CREATE TABLE t(id INT, v INT)",
        "INSERT INTO t VALUES(1,10),(2,20)",
        "CREATE TABLE s(id INT, add_v INT)",
        "INSERT INTO s VALUES(1,5),(2,7)",
    ]);
    c.execute("UPDATE t SET v = v + s.add_v FROM s WHERE t.id = s.id")
        .unwrap();
    assert_eq!(
        pairs(&c, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 15), (2, 27)]
    );
}

#[test]
fn multiple_from_tables_and_derived_source() {
    let mut c = conn(&[
        "CREATE TABLE t(id INT, v INT)",
        "INSERT INTO t VALUES(1,0)",
        "CREATE TABLE a(id INT, x INT)",
        "INSERT INTO a VALUES(1,5)",
        "CREATE TABLE b(id INT, y INT)",
        "INSERT INTO b VALUES(1,7)",
    ]);
    c.execute("UPDATE t SET v = a.x + b.y FROM a, b WHERE t.id = a.id AND a.id = b.id")
        .unwrap();
    assert_eq!(pairs(&c, "SELECT id, v FROM t"), vec![(1, 12)]);

    let mut c = conn(&[
        "CREATE TABLE t(id INT, v INT)",
        "INSERT INTO t VALUES(1,10),(2,20)",
    ]);
    c.execute("UPDATE t SET v = d.s FROM (SELECT 1 AS id, 500 AS s) d WHERE t.id = d.id")
        .unwrap();
    assert_eq!(
        pairs(&c, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, 500), (2, 20)]
    );
}

#[test]
fn update_from_fires_triggers() {
    let mut c = conn(&[
        "CREATE TABLE t(id INT, v INT)",
        "INSERT INTO t VALUES(1,10)",
        "CREATE TABLE s(id INT, nv INT)",
        "INSERT INTO s VALUES(1,99)",
        "CREATE TABLE log(o INT, n INT)",
        "CREATE TRIGGER tr AFTER UPDATE ON t BEGIN INSERT INTO log VALUES(OLD.v, NEW.v); END",
    ]);
    c.execute("UPDATE t SET v = s.nv FROM s WHERE t.id = s.id")
        .unwrap();
    assert_eq!(pairs(&c, "SELECT o, n FROM log"), vec![(10, 99)]);
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let script = "CREATE TABLE t(id INT, v INT); INSERT INTO t VALUES(1,10),(2,20),(3,30); \
                  CREATE TABLE s(id INT, nv INT); INSERT INTO s VALUES(1,100),(3,300); \
                  UPDATE t SET v = s.nv FROM s WHERE t.id = s.id; \
                  SELECT id||':'||v FROM t ORDER BY id;";
    let want = {
        let o = Command::new("sqlite3")
            .arg(":memory:")
            .arg(script)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    let mut c = conn(&[
        "CREATE TABLE t(id INT, v INT)",
        "INSERT INTO t VALUES(1,10),(2,20),(3,30)",
        "CREATE TABLE s(id INT, nv INT)",
        "INSERT INTO s VALUES(1,100),(3,300)",
        "UPDATE t SET v = s.nv FROM s WHERE t.id = s.id",
    ]);
    let got: Vec<String> = c
        .query("SELECT id||':'||v FROM t ORDER BY id")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(t) => String::from(t.as_str()),
            _ => String::new(),
        })
        .collect();
    assert_eq!(got.join("\n"), want);
    let _ = &mut c;
}
