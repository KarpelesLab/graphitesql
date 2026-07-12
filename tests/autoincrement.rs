//! AUTOINCREMENT and the `sqlite_sequence` catalog: an `INTEGER PRIMARY KEY
//! AUTOINCREMENT` never reuses a rowid below the high-water mark persisted in
//! `sqlite_sequence`. Verified against the `sqlite3` CLI (including cross-engine:
//! sqlite3 reads graphite's file and continues the sequence).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => String::from(s.as_str()),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn rows(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn autoincrement_does_not_reuse_after_delete() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, a)")
        .unwrap();
    c.execute("INSERT INTO t(a) VALUES('x'),('y'),('z')")
        .unwrap();
    c.execute("DELETE FROM t").unwrap();
    c.execute("INSERT INTO t(a) VALUES('w')").unwrap();
    // The new row continues from 4, not 1 (a plain INTEGER PRIMARY KEY would reuse).
    assert_eq!(rows(&c, "SELECT id FROM t"), "4");
    assert_eq!(rows(&c, "SELECT name, seq FROM sqlite_sequence"), "t|4");

    // A plain (non-AUTOINCREMENT) rowid table reuses the freed maximum.
    let mut p = Connection::open_memory().unwrap();
    p.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, a)")
        .unwrap();
    p.execute("INSERT INTO u(a) VALUES('x')").unwrap();
    p.execute("DELETE FROM u").unwrap();
    p.execute("INSERT INTO u(a) VALUES('y')").unwrap();
    assert_eq!(rows(&p, "SELECT id FROM u"), "1");
}

#[test]
fn autoincrement_matches_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // (script, query) pairs; both run on a fresh db.
    let cases = [
        // sqlite_sequence is created (empty) by the AUTOINCREMENT table's CREATE.
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, a)",
            "SELECT count(*) FROM sqlite_sequence; SELECT name FROM sqlite_master WHERE name='sqlite_sequence'",
        ),
        // High-water survives a delete of the maximum.
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, a); INSERT INTO t(a) VALUES('a'),('b'),('c'); DELETE FROM t WHERE id=3; INSERT INTO t(a) VALUES('d')",
            "SELECT id FROM t ORDER BY id; SELECT seq FROM sqlite_sequence",
        ),
        // An explicit larger rowid advances the counter.
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, a); INSERT INTO t VALUES(100,'x'); INSERT INTO t(a) VALUES('y')",
            "SELECT id FROM t ORDER BY id; SELECT seq FROM sqlite_sequence",
        ),
        // Two AUTOINCREMENT tables get independent rows; DROP removes one.
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT,a); CREATE TABLE u(id INTEGER PRIMARY KEY AUTOINCREMENT,a); INSERT INTO t(a) VALUES('x'); INSERT INTO u(a) VALUES('y'); DROP TABLE t",
            "SELECT name, seq FROM sqlite_sequence ORDER BY name",
        ),
    ];
    for (script, query) in cases {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(format!("{script}; {query};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let mut c = Connection::open_memory().unwrap();
        for s in script.split(';') {
            if !s.trim().is_empty() {
                c.execute(s).unwrap();
            }
        }
        // The query may be several `;`-separated statements; run and join like the CLI.
        let got = query
            .split(';')
            .filter(|s| !s.trim().is_empty())
            .map(|q| rows(&c, q.trim()))
            .collect::<Vec<_>>()
            .join("\n");
        let got = got.trim_end().to_string();
        assert_eq!(got, want, "diverged for: {script}");
    }
}

#[test]
fn autoincrement_file_is_cross_engine() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-autoinc-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, a)")
            .unwrap();
        c.execute("INSERT INTO t(a) VALUES('a'),('b'),('c')")
            .unwrap();
        c.execute("DELETE FROM t WHERE id=3").unwrap();
    }
    // sqlite3 reads graphite's file: integrity ok, sequence preserved, and a new
    // insert continues from the high-water mark (4, not 3).
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check; INSERT INTO t(a) VALUES('d'); SELECT id FROM t WHERE a='d';")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("ok"), "integrity: {s}");
    assert!(s.contains('4'), "sqlite did not continue the sequence: {s}");
    let _ = std::fs::remove_file(&path);
}
