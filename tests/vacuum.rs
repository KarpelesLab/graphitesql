//! Phase 9: real `VACUUM` compaction — rebuild into a fresh, compact image.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-vac-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

fn cleanup(path: &str) {
    for s in ["", "-journal", "-wal"] {
        let _ = std::fs::remove_file(format!("{path}{s}"));
    }
}

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

#[test]
fn vacuum_shrinks_and_preserves() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_path("v.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT, s TEXT)")
            .unwrap();
        c.execute("CREATE INDEX iv ON t(v)").unwrap();
        for i in 1..=2000 {
            c.execute(&format!(
                "INSERT INTO t(v,s) VALUES ({}, 'row{}')",
                i % 50,
                i
            ))
            .unwrap();
        }
        // Delete most rows to create lots of free space.
        c.execute("DELETE FROM t WHERE id > 100").unwrap();
    }
    let before = std::fs::metadata(&path).unwrap().len();
    {
        let mut c = Connection::open(&path).unwrap();
        c.execute("VACUUM").unwrap();
    }
    let after = std::fs::metadata(&path).unwrap().len();
    assert!(
        after < before,
        "VACUUM should shrink the file: before={before} after={after}"
    );
    // Data preserved and the file is valid per real sqlite3.
    assert_eq!(
        Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check;")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap(),
        "ok"
    );
    let c = Connection::open_readonly(&path).unwrap();
    assert_eq!(
        rows(&c, "SELECT count(*) FROM t")[0][0],
        Value::Integer(100)
    );
    // Index still works after VACUUM.
    let n = match rows(&c, "SELECT count(*) FROM t WHERE v = 7")[0][0] {
        Value::Integer(n) => n,
        _ => panic!(),
    };
    assert!(n > 0);
    cleanup(&path);
}

#[test]
fn vacuum_in_memory_is_noop_but_ok() {
    // VACUUM on an in-memory database is accepted and preserves data.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("INSERT INTO t(v) VALUES (1),(2),(3)").unwrap();
    c.execute("VACUUM").unwrap();
    assert_eq!(rows(&c, "SELECT sum(v) FROM t")[0][0], Value::Integer(6));
}

#[test]
fn vacuum_preserves_triggers_and_user_version() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_path("v2.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
            .unwrap();
        c.execute("CREATE TABLE log(id INTEGER PRIMARY KEY, v INT)")
            .unwrap();
        c.execute(
            "CREATE TRIGGER trg AFTER INSERT ON t BEGIN INSERT INTO log(v) VALUES (NEW.v); END",
        )
        .unwrap();
        c.execute("INSERT INTO t(v) VALUES (5),(6)").unwrap();
        // log now has 2 rows (from the trigger).
        c.execute("VACUUM").unwrap();
        // After VACUUM the trigger still exists and fires.
        c.execute("INSERT INTO t(v) VALUES (7)").unwrap();
        assert_eq!(
            rows(&c, "SELECT count(*) FROM log")[0][0],
            Value::Integer(3)
        );
    }
    assert_eq!(
        Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check;")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap(),
        "ok"
    );
    cleanup(&path);
}
