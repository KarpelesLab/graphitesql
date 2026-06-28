//! Row-level `SELECT DISTINCT` with an *explicit* `COLLATE` on a projection
//! column must dedup under that collation. The VDBE's `DistinctCheck` compares
//! output rows under BINARY and the per-path bail only inspects a column's
//! *declared* collation, so an explicit `COLLATE` (`SELECT DISTINCT a COLLATE
//! NOCASE`) has to defer the whole query to the tree-walker. An explicit
//! `COLLATE BINARY` is BINARY already and keeps running on the VDBE. Verified
//! against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT, b INT)").unwrap();
    c.execute("INSERT INTO t VALUES ('x',1),('X',1),('y',2)")
        .unwrap();
    c
}

#[test]
fn distinct_with_explicit_collate_defers_but_is_correct() {
    let c = conn();
    // Explicit non-BINARY collation in the projection: the VDBE must defer.
    for q in [
        "SELECT DISTINCT a COLLATE NOCASE FROM t",
        "SELECT DISTINCT a COLLATE NOCASE, b FROM t",
        "SELECT DISTINCT (a COLLATE NOCASE) FROM t",
    ] {
        assert!(c.query_vdbe(q).is_err(), "expected VDBE to defer on {q}");
    }
    // The default path (tree-walker fallback) collapses 'x'=='X' under NOCASE.
    assert_eq!(
        c.query("SELECT DISTINCT a COLLATE NOCASE FROM t ORDER BY 1")
            .unwrap()
            .rows,
        vec![vec![Value::Text("x".into())], vec![Value::Text("y".into())]],
    );

    // Explicit COLLATE BINARY is the default comparison and stays on the VDBE:
    // 'x' and 'X' remain distinct.
    let r = c
        .query_vdbe("SELECT DISTINCT a COLLATE BINARY FROM t ORDER BY 1")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Text("X".into())],
            vec![Value::Text("x".into())],
            vec![Value::Text("y".into())],
        ],
    );
}

#[test]
fn distinct_with_explicit_collate_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    let setup = "CREATE TABLE t(a TEXT, b INT);\nINSERT INTO t VALUES ('x',1),('X',1),('y',2);\n";
    for q in [
        "SELECT DISTINCT a COLLATE NOCASE FROM t ORDER BY 1",
        "SELECT DISTINCT a COLLATE NOCASE, b FROM t ORDER BY 1, 2",
        "SELECT DISTINCT a COLLATE BINARY FROM t ORDER BY 1",
    ] {
        let got: Vec<Vec<String>> = c
            .query(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| match v {
                        Value::Null => String::new(),
                        Value::Integer(i) => i.to_string(),
                        Value::Text(s) => s.clone(),
                        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                    })
                    .collect()
            })
            .collect();
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg("-ascii")
            .arg(format!("{setup}{q};"))
            .output()
            .unwrap();
        assert!(out.status.success(), "sqlite3 failed on {q}");
        let text = String::from_utf8(out.stdout).unwrap();
        let expected: Vec<Vec<String>> = text
            .split('\u{1e}')
            .filter(|r| !r.is_empty())
            .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
            .collect();
        assert_eq!(got, expected, "diverged on {q}");
    }
}
