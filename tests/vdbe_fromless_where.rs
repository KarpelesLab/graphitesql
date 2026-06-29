//! A `FROM`-less `SELECT … WHERE <pred>` runs on the VDBE: the predicate is
//! evaluated once over the single rowless row, and the projection is emitted only
//! when it is true (a false or NULL predicate yields zero rows). Previously the
//! constant-SELECT compiler bailed on any `WHERE`, deferring to the tree-walker.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the call
//! compiled. Results are checked against the tree-walker and the sqlite3 3.50.4
//! CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const QUERIES: &[&str] = &[
    // True predicate → one row.
    "SELECT 1 WHERE 1 IN (1,2,3)",
    // False predicate → zero rows.
    "SELECT 1 WHERE 0",
    // NULL predicate → zero rows (treated as false).
    "SELECT 1 WHERE NULL",
    // Multi-column projection, comparison predicate.
    "SELECT 1, 2, 3 WHERE 'a' = 'a'",
    // Conjunction that is false → zero rows.
    "SELECT 'hi' WHERE 5 > 3 AND 2 < 1",
    // A function in the projection, gated by a true predicate.
    "SELECT abs(-4) WHERE 1",
    // A function in the predicate itself.
    "SELECT upper('x') AS u WHERE length('abc') = 3",
    // BETWEEN predicate, true.
    "SELECT 'yes' WHERE 5 BETWEEN 1 AND 10",
    // Predicate referencing only literals via OR.
    "SELECT 42 WHERE 0 OR 1",
    // A CASE expression as the predicate.
    "SELECT 'ok' WHERE CASE WHEN 1 THEN 1 ELSE 0 END",
    // String comparison that is false.
    "SELECT 1 WHERE 'b' < 'a'",
    // Arithmetic predicate.
    "SELECT 7 WHERE 2 + 2 = 4",
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
fn fromless_where_runs_on_vdbe_and_matches_tree_walker() {
    let c = Connection::open_memory().unwrap();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the call compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn fromless_where_matches_sqlite3() {
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
