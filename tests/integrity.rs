//! Track C: in-engine `PRAGMA integrity_check` / `quick_check`. For every valid
//! database graphitesql builds, its own check must agree with `sqlite3`'s.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn graphite_check(c: &Connection) -> String {
    match &c.query("PRAGMA integrity_check").unwrap().rows[0][0] {
        Value::Text(s) => String::from(s.as_str()),
        _ => "?".into(),
    }
}

#[test]
fn ok_on_valid_databases() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let schemas: &[&[&str]] = &[
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, k INT, v TEXT)",
            "CREATE INDEX ik ON t(k)",
            "CREATE INDEX ikv ON t(k, v)",
            "INSERT INTO t(k,v) VALUES (1,'a'),(2,'b'),(2,'c'),(3,'d')",
        ],
        &[
            "CREATE TABLE t(a, b TEXT COLLATE NOCASE, c)",
            "CREATE UNIQUE INDEX iu ON t(b)",
            "CREATE INDEX ip ON t(c) WHERE a > 0",
            "CREATE INDEX ie ON t(lower(b))",
            "INSERT INTO t VALUES (1,'X',5),(-1,'y',6),(2,'Z',-3)",
        ],
        &[
            "CREATE TABLE wr(k TEXT PRIMARY KEY, v) WITHOUT ROWID",
            "CREATE INDEX iv ON wr(v)",
            "INSERT INTO wr VALUES ('a',1),('b',2),('c',3)",
        ],
    ];

    for (i, schema) in schemas.iter().enumerate() {
        let path = std::env::temp_dir().join(format!("gsql-ic-{}-{i}.db", std::process::id()));
        let path = path.to_string_lossy().into_owned();
        let _ = std::fs::remove_file(&path);
        // Build with graphitesql, then check with both engines.
        {
            let mut c = Connection::create(&path).unwrap();
            for s in *schema {
                c.execute(s).unwrap();
            }
            assert_eq!(
                graphite_check(&c),
                "ok",
                "graphite check failed on schema {i}"
            );
        }
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check;")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "ok",
            "sqlite3 check failed on schema {i}"
        );
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn ok_in_memory() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, k)")
        .unwrap();
    c.execute("CREATE INDEX ik ON t(k)").unwrap();
    c.execute("INSERT INTO t(k) VALUES (1),(2),(3)").unwrap();
    c.execute("DELETE FROM t WHERE k = 2").unwrap();
    c.execute("UPDATE t SET k = 9 WHERE k = 3").unwrap();
    assert_eq!(graphite_check(&c), "ok");
    // quick_check is an alias.
    match &c.query("PRAGMA quick_check").unwrap().rows[0][0] {
        Value::Text(s) => assert_eq!(s, "ok"),
        _ => panic!("not text"),
    }
}
