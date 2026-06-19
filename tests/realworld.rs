//! Phase 9: read a database whose schema uses the full constraint grammar
//! (CHECK, REFERENCES, FOREIGN KEY, named CONSTRAINT) — graphitesql must parse
//! the stored `CREATE TABLE` text to resolve columns, so this exercises parser
//! robustness against real-world schemas.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

#[test]
fn reads_database_with_fk_and_check_constraints() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let mut path = std::env::temp_dir();
    path.push(format!("graphitesql-rw-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    // Build with the real sqlite3, using constraints we accept-and-skip.
    let build = Command::new("sqlite3")
        .arg(&path)
        .arg(
            "PRAGMA foreign_keys=ON;\
             CREATE TABLE parent(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);\
             CREATE TABLE child(\
                id INTEGER PRIMARY KEY,\
                pid INT REFERENCES parent(id) ON DELETE CASCADE,\
                qty INT NOT NULL CHECK(qty > 0),\
                CONSTRAINT uq UNIQUE(pid, qty));\
             INSERT INTO parent(name) VALUES ('a'),('b');\
             INSERT INTO child(pid, qty) VALUES (1,5),(1,9),(2,3);",
        )
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );

    // graphitesql must read it (parsing the constrained CREATE TABLE text).
    let c = Connection::open_readonly(&path).unwrap();
    assert!(c.schema().table("child").is_some());

    let r = c
        .query("SELECT qty FROM child WHERE pid = 1 ORDER BY qty")
        .unwrap();
    let qtys: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            _ => panic!(),
        })
        .collect();
    assert_eq!(qtys, vec![5, 9]);

    let r = c
        .query("SELECT parent.name, sum(child.qty) FROM parent JOIN child ON parent.id = child.pid GROUP BY parent.name ORDER BY parent.name")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Text("a".into()));
    assert_eq!(r.rows[0][1], Value::Integer(14)); // 5 + 9

    let _ = std::fs::remove_file(&path);
}
