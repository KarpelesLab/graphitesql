//! Differential test for a `NOT INDEXED` single-table `SELECT` running on the
//! VDBE. `NOT INDEXED` forbids using any index — i.e. forces a full table scan —
//! which is exactly what the VDBE scan does, so the query now runs on the VDBE
//! (previously it bailed to the tree-walker with every other index hint). The
//! rows and their order must match the real `sqlite3` CLI, which also full-scans.
//! (`INDEXED BY name` still defers — it must be honoured or rejected.)

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3(setup: &[&str], query: &str) -> String {
    let mut script = String::new();
    for s in setup {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push(';');
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(&script)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn render(result: &graphitesql::QueryResult) -> String {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Text(s) => String::from(s.as_str()),
                    Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                    Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn not_indexed_scan_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }

    // Rows inserted out of key order, plus an index on `a` that a plan could
    // otherwise use — so `NOT INDEXED` (full scan) yields rowid order, and the
    // VDBE's full scan must reproduce it.
    let setup = [
        "CREATE TABLE t(a INTEGER, b TEXT)",
        "CREATE INDEX ix ON t(a)",
        "INSERT INTO t VALUES(3,'c'),(1,'a'),(2,'b'),(1,'d')",
    ];

    let mut g = Connection::open_memory().unwrap();
    for s in &setup {
        g.execute(s).unwrap();
    }

    let queries = [
        "SELECT a, b FROM t NOT INDEXED",           // full scan, rowid order
        "SELECT a FROM t NOT INDEXED WHERE a >= 2", // filtered full scan
        "SELECT a, b FROM t NOT INDEXED WHERE a = 1", // equality (no index used)
        "SELECT a FROM t NOT INDEXED ORDER BY a, b", // explicit sort
        "SELECT count(*), sum(a), max(b) FROM t NOT INDEXED", // aggregates
        "SELECT DISTINCT a FROM t NOT INDEXED ORDER BY 1", // DISTINCT
        "SELECT b FROM t NOT INDEXED LIMIT 2 OFFSET 1", // LIMIT/OFFSET over scan
    ];

    for q in queries {
        let want = sqlite3(&setup, q);
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "NOT INDEXED scan diverged: {q}");
    }
}
