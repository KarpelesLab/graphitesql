//! A derived table (`FROM (SELECT … UNION/UNION ALL/INTERSECT/EXCEPT SELECT …)
//! alias`) whose body is a compound now runs on the VDBE when **every arm yields the
//! same `(affinity, collation)` for each column**. The body materializes through
//! `run_select` (the same rows the tree-walker produces); the new part is that
//! `subquery_column_origins` resolves the result column's origin to the arms' shared
//! one — so an affinity-sensitive outer `WHERE` / `ORDER BY` over a derived column
//! coerces exactly as SQLite does. (Two `INTEGER` arms keep INTEGER affinity, so
//! `v = '2'` matches the integer 2 — previously the conservative BLOB default made
//! the tree-walker drop the row, a real divergence this also fixes.)
//!
//! Deferred to the tree-walker (asserted separately), never run wrong:
//!   * arms whose per-column affinity / collation *disagree* (`INTEGER` UNION `TEXT`)
//!     — SQLite uses no affinity there, which the conservative default already
//!     matches, so the VDBE declines rather than guess a combine rule.
//!   * a FROM-less arm (`… UNION SELECT 99`) or a nested compound arm.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE handled
//! the compound derived source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES (1,'x',10),(2,'y',20),(3,'z',30);\n\
    CREATE TABLE u(g INTEGER, m INTEGER);\n\
    INSERT INTO u VALUES (1,100),(2,200),(2,201);\n";

// Each query's FROM source is a derived table whose body is a compound whose arms
// share each column's affinity. ORDER BY (or a deterministic aggregate) pins the
// row order for a direct comparison.
const QUERIES: &[&str] = &[
    // Affinity-sensitive outer WHERE: both arms INTEGER, so `v = '2'` coerces and
    // matches (the BLOB default would drop it — the divergence this fixes).
    "SELECT v FROM (SELECT g AS v FROM t UNION SELECT g FROM u) x WHERE v = '2' ORDER BY 1",
    // UNION ALL of two INTEGER columns, numeric ORDER BY.
    "SELECT v FROM (SELECT g AS v FROM t UNION ALL SELECT m FROM u) x ORDER BY v",
    // UNION (distinct) of two INTEGER columns.
    "SELECT v FROM (SELECT g AS v FROM t UNION SELECT m FROM u) x ORDER BY 1",
    // Two TEXT arms keep TEXT affinity.
    "SELECT v FROM (SELECT a AS v FROM t UNION SELECT a FROM t) x ORDER BY 1",
    // INTERSECT / EXCEPT bodies (same INTEGER affinity).
    "SELECT v FROM (SELECT g AS v FROM t INTERSECT SELECT g FROM u) x ORDER BY 1",
    "SELECT v FROM (SELECT g AS v FROM t EXCEPT SELECT g FROM u) x ORDER BY 1",
    // Multi-column compound: both columns share affinity across arms.
    "SELECT a1, b1 FROM (SELECT g AS a1, n AS b1 FROM t UNION SELECT g, m FROM u) x ORDER BY 1,2",
    // Aggregate over the compound body.
    "SELECT count(*), sum(v) FROM (SELECT g AS v FROM t UNION ALL SELECT m FROM u) x",
    // Affinity-sensitive WHERE over a UNION of two INTEGER columns.
    "SELECT v FROM (SELECT n AS v FROM t UNION SELECT m FROM u) x WHERE v = '20' ORDER BY 1",
    // Three INTEGER arms.
    "SELECT v FROM (SELECT g AS v FROM t UNION SELECT m FROM u UNION SELECT n FROM t) x \
     ORDER BY 1",
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
fn derived_compound_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE scanned the body.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn derived_compound_matches_sqlite3() {
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

#[test]
fn mixed_affinity_and_fromless_arms_defer() {
    let c = conn();
    // Arms whose per-column affinity disagrees (INTEGER vs TEXT), a FROM-less arm,
    // and a nested compound arm each defer to the tree-walker, which still runs them
    // (and already matches SQLite, since it uses no affinity for mixed arms).
    for q in [
        "SELECT v FROM (SELECT g AS v FROM t UNION ALL SELECT a FROM t) x WHERE v = '2'",
        "SELECT v FROM (SELECT a AS v FROM t UNION ALL SELECT g FROM t) x ORDER BY 1",
        "SELECT v FROM (SELECT g AS v FROM t UNION SELECT 99) x ORDER BY 1",
    ] {
        assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
        assert!(c.query(q).is_ok(), "tree-walker should run {q}");
    }
}

// The same-affinity compound body now coerces an outer text predicate exactly as
// SQLite does — a regression guard for the affinity propagation through arms (before
// the fix the tree-walker used the BLOB default and returned no rows).
#[test]
fn same_affinity_compound_coerces_outer_predicate() {
    let c = conn();
    let q = "SELECT v FROM (SELECT g AS v FROM t UNION SELECT g FROM u) x WHERE v = '2'";
    let rows = c.query(q).unwrap().rows;
    assert_eq!(
        rows,
        vec![vec![Value::Integer(2)]],
        "compound INTEGER affinity must coerce the text '2' to match"
    );
    // And it runs on the VDBE identically.
    assert_eq!(c.query_vdbe(q).unwrap().rows, rows);
}
