//! Track A: expression indexes (`CREATE INDEX … (expr)`). The index is keyed by
//! the evaluated expression per row; verified by `sqlite3`'s `integrity_check`,
//! which recomputes the expression and checks the entries.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn integrity(path: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(path)
        .arg("PRAGMA integrity_check;")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn expr_index_integrity() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-eidx-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, a INT, b INT)")
            .unwrap();
        c.execute("CREATE INDEX i_lower ON t(lower(name))").unwrap();
        c.execute("CREATE INDEX i_sum ON t(a + b)").unwrap();
        for i in 0..25 {
            c.execute(&format!(
                "INSERT INTO t(id,name,a,b) VALUES ({i},'Name{}',{i},{})",
                i % 5,
                i * 3
            ))
            .unwrap();
        }
        c.execute("UPDATE t SET name='ZZZ', a=100 WHERE id=2")
            .unwrap();
        c.execute("DELETE FROM t WHERE id=0").unwrap();
    }
    assert_eq!(integrity(&path), "ok");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn expr_index_query_results_correct() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    c.execute("CREATE INDEX i ON t(lower(name))").unwrap();
    c.execute("INSERT INTO t(name) VALUES ('Alice'),('BOB'),('alice')")
        .unwrap();
    // Querying through the expression returns correct rows (planner scans).
    assert_eq!(
        c.query("SELECT count(*) FROM t WHERE lower(name) = 'alice'")
            .unwrap()
            .rows[0][0],
        Value::Integer(2)
    );
}
