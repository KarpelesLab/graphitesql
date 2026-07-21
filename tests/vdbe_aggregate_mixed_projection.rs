//! A bare-aggregate `SELECT` (no GROUP BY) whose projection is more than a single
//! aggregate call now runs on the VDBE. `compile_aggregate_select` became
//! binding-driven: it folds every distinct aggregate referenced by the projection
//! (and HAVING) into a slot, then compiles each projection expression against those
//! finalized registers. So a constant column beside an aggregate (`SELECT 'total',
//! count(*)`), an aggregate wrapped in arithmetic (`SELECT sum(a)*2`), several
//! aggregates in one term (`SELECT sum(a)+count(*)`), and a CASE/`||` over an
//! aggregate all run — the router now routes on a *deep* aggregate check
//! (`expr_has_aggregate`), not just a top-level `count(*)`.
//!
//! A bare (non-aggregate) column in the projection has no per-row value in a bare
//! aggregate, so it still defers (asserted). `query_vdbe` errors on any fallback, so
//! a passing query proves the VDBE ran it. Checked against the tree-walker and
//! sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(a INTEGER, b TEXT);\n\
    INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'z'),(2,'y');\n";

const QUERIES: &[&str] = &[
    // A constant column beside an aggregate (either order).
    "SELECT 'total', count(*) FROM t",
    "SELECT count(*), 'label' FROM t",
    "SELECT 1 + 1, sum(a) FROM t",
    "SELECT count(*), 5, sum(a) FROM t",
    // An aggregate wrapped in arithmetic.
    "SELECT sum(a) * 2 FROM t",
    "SELECT max(a) - min(a) AS range FROM t",
    "SELECT round(avg(a), 1) FROM t",
    // Several aggregates in one term.
    "SELECT sum(a) + count(*) FROM t",
    "SELECT sum(a) / count(*) FROM t",
    // A string / CASE built over an aggregate.
    "SELECT 'n=' || count(*) FROM t",
    "SELECT CASE WHEN count(*) > 2 THEN 'many' ELSE 'few' END FROM t",
    // DISTINCT aggregate inside an expression.
    "SELECT count(DISTINCT b) * 10 FROM t",
    "SELECT count(DISTINCT b), count(*) FROM t",
    // Mixed with WHERE and HAVING.
    "SELECT 'r', count(*) FROM t WHERE a > 1",
    "SELECT sum(a) * 2 FROM t HAVING count(*) > 1",
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
fn mixed_projection_aggregate_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE folded and emitted.
        let got = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE to run {q}: {e}"));
        let want = c.query(q).unwrap().rows;
        assert_eq!(got.rows, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn bare_column_in_bare_aggregate_still_defers() {
    let c = conn();
    // A bare (non-aggregate) column in a bare aggregate has no per-row value, so the
    // VDBE defers and the tree-walker (which picks an arbitrary row, per SQLite)
    // handles it.
    for q in ["SELECT b, count(*) FROM t", "SELECT a, sum(a) FROM t"] {
        assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    }
}

#[test]
fn mixed_projection_aggregate_matches_sqlite3() {
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
