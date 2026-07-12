//! `SELECT *` (and `tbl.*`) over a join whose sources **share a column name** now runs
//! on the VDBE. The wildcard expansion qualifies each expanded column with its source
//! table (from the scan's parallel `tables` slice), so `SELECT * FROM t JOIN u` where
//! both carry `g` resolves each `g` to its own source instead of erroring on an
//! ambiguous bare reference — `*` expands to every column exactly as the tree-walker
//! and SQLite do. The output labels stay the bare column names.
//!
//! This also unlocks **window functions over such a join** (the window base scan is a
//! `SELECT *` over the same sources), including a join that contains a view.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE handled the
//! wildcard join. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES (1,'x',10),(2,'y',20),(3,'z',30);\n\
    CREATE TABLE u(g INTEGER, m INTEGER);\n\
    INSERT INTO u VALUES (1,100),(2,200),(2,201);\n\
    CREATE VIEW vt AS SELECT g, n FROM t;\n";

// Each query expands a wildcard over a join whose sources share the column name `g`.
const QUERIES: &[&str] = &[
    // Two plain tables sharing `g` — the bare-name ambiguity this fixes.
    "SELECT * FROM t JOIN u ON t.g = u.g ORDER BY t.g, u.m",
    // A comma (implicit cross) join, also sharing `g`.
    "SELECT * FROM t, u ORDER BY t.g, u.g, u.m",
    // A LEFT join (null-padded rows flow through).
    "SELECT * FROM t LEFT JOIN u ON t.g = u.g ORDER BY t.g, u.m",
    // `t.*` plus an explicit column from the other source.
    "SELECT t.*, u.m FROM t JOIN u ON t.g = u.g ORDER BY t.g, u.m",
    // A join containing a view, sharing `g`.
    "SELECT * FROM t JOIN vt ON t.g = vt.g ORDER BY t.g",
    // Wildcard over a colliding join with an outer affinity-sensitive predicate.
    "SELECT * FROM t JOIN u ON t.g = u.g WHERE t.g = '2' ORDER BY u.m",
    // A window function over the colliding plain join (base scan is a `SELECT *`).
    "SELECT t.g, u.m, sum(u.m) OVER (PARTITION BY t.g) FROM t JOIN u ON t.g = u.g \
     ORDER BY 1, 2",
];

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    c
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

#[test]
fn join_wildcard_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn join_wildcard_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    for q in QUERIES {
        let vdbe: Vec<Vec<String>> = c
            .query_vdbe(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| r.iter().map(render).collect())
            .collect();
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg("-ascii")
            .arg(format!("{SETUP}{q};"))
            .output()
            .unwrap();
        assert!(out.status.success(), "sqlite3 failed on {q}");
        let text = String::from_utf8(out.stdout).unwrap();
        let want: Vec<Vec<String>> = text
            .split('\u{1e}')
            .filter(|r| !r.is_empty())
            .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
            .collect();
        assert_eq!(vdbe, want, "VDBE vs sqlite3 diverged on {q}");
    }
}
