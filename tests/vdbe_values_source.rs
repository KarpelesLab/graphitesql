//! A constant / `VALUES` subquery in the `FROM` clause now runs on the VDBE.
//! A top-level `VALUES (…),(…)` desugars to a `UNION ALL` of FROM-less constant
//! cores, so as a derived table it is materialized directly: its columns have no
//! affinity and BINARY collation, exactly matching the tree-walker and SQLite.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE
//! handled the derived source. Results are checked against both the tree-walker
//! and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

// No base tables — each query stands alone. ORDER BY keys are deterministic so
// the row order is stable for a direct comparison.
const QUERIES: &[&str] = &[
    "SELECT * FROM (VALUES (1,2),(3,4))",
    "SELECT column1, column2 FROM (VALUES (1,2),(3,4)) ORDER BY column1 DESC",
    "SELECT * FROM (VALUES (1,'a'),(2,'b'),(3,'c')) WHERE column1 > 1 ORDER BY column1",
    "SELECT count(*), sum(column1) FROM (VALUES (1),(2),(3),(4))",
    "SELECT column1 * 10 FROM (VALUES (1),(2),(3)) ORDER BY column1",
    "SELECT * FROM (SELECT 10, 'hi')",
    "SELECT * FROM (VALUES (1,2),(3,4)) AS v ORDER BY 1",
    "SELECT max(column1), min(column1) FROM (VALUES (5),(1),(9),(3))",
    "SELECT column2 FROM (VALUES (1,'x'),(2,'y'),(3,'z')) WHERE column1 <> 2 ORDER BY column1",
    "SELECT column1 FROM (VALUES (3),(1),(2)) ORDER BY column1 LIMIT 2",
];

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

#[test]
fn values_source_runs_on_vdbe_and_matches_tree_walker() {
    let c = Connection::open_memory().unwrap();
    for q in QUERIES {
        // `query_vdbe` errors on fallback; a deterministic order makes the rows
        // directly comparable against the tree-walker oracle.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn values_source_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = Connection::open_memory().unwrap();
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
            .arg(format!("{q};"))
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
