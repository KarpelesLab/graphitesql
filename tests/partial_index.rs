//! Track A: partial indexes (`CREATE INDEX … WHERE`). The index stores only the
//! rows matching its predicate; verified by `sqlite3`'s `integrity_check`, which
//! reports a wrong entry count if the partial set is incorrect.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn integrity_ok(path: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(path)
        .arg("PRAGMA integrity_check;")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn partial_index_integrity() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-pidx-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, status TEXT, v INT)")
            .unwrap();
        c.execute("CREATE INDEX i_active ON t(v) WHERE status = 'active'")
            .unwrap();
        for i in 0..30 {
            let status = if i % 3 == 0 { "active" } else { "done" };
            c.execute(&format!(
                "INSERT INTO t(id,status,v) VALUES ({i},'{status}',{})",
                i * 2
            ))
            .unwrap();
        }
        // Updates that move rows across the predicate boundary both ways.
        c.execute("UPDATE t SET status='active' WHERE id=1")
            .unwrap();
        c.execute("UPDATE t SET status='done' WHERE id=3").unwrap();
        c.execute("DELETE FROM t WHERE id=0").unwrap();
    }
    assert_eq!(integrity_ok(&path), "ok");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn results_correct_with_partial_index() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, status TEXT, v INT)")
        .unwrap();
    c.execute("CREATE INDEX i ON t(v) WHERE status = 'active'")
        .unwrap();
    c.execute("INSERT INTO t VALUES (1,'active',10),(2,'done',10),(3,'active',20)")
        .unwrap();
    // A query on v returns all matching rows regardless of the partial predicate
    // (the planner falls back to a scan rather than misusing the index).
    assert_eq!(
        c.query("SELECT count(*) FROM t WHERE v = 10").unwrap().rows[0][0],
        Value::Integer(2)
    );
    assert_eq!(
        c.query("SELECT id FROM t WHERE v = 20").unwrap().rows[0][0],
        Value::Integer(3)
    );
}

#[test]
fn partial_index_against_sqlite3_data() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Build the same database in graphitesql and sqlite3; the visible data must
    // match (the index is an internal optimization).
    let setup = [
        "CREATE TABLE t(id INTEGER PRIMARY KEY, k INT, flag INT)",
        "CREATE INDEX ipos ON t(k) WHERE flag = 1",
        "INSERT INTO t(id,k,flag) VALUES (1,5,1),(2,3,0),(3,9,1),(4,1,1),(5,7,0)",
        "UPDATE t SET flag=1 WHERE id=2",
    ];
    let path = std::env::temp_dir().join(format!("gsql-pidx2-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(setup.join(";"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let want = {
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg("SELECT id,k,flag FROM t ORDER BY id")
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    };
    let _ = std::fs::remove_file(&path);

    let mut g = Connection::open_memory().unwrap();
    for s in setup {
        g.execute(s).unwrap();
    }
    let got = g
        .query("SELECT id,k,flag FROM t ORDER BY id")
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Integer(i) => i.to_string(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(got, want);
}
