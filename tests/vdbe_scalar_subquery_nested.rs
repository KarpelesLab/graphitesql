//! A non-correlated scalar subquery (or `EXISTS` / `IN (SELECT …)`) whose own
//! body contains a *further* nested subquery now folds to a constant and runs on
//! the VDBE — provided every nested subquery is itself self-contained against the
//! accumulated scope. The fold walks the subquery's scope: a nested body may reach
//! into its parent's sources (correlation that stays inside the folded unit), but
//! a reference further out (genuine correlation to the outermost row) still defers
//! to the tree-walker. The whole unit is evaluated by the tree-walker, so affinity
//! and collation semantics of the nested subquery are preserved exactly.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE folded
//! the subquery. A correlated subquery must still fall back. Checked against the
//! tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'x',10),(1,'y',20),\n\
      (2,'z',5),(2,'w',7),\n\
      (3,'q',100);\n";

// Each query embeds a non-correlated scalar subquery whose body itself contains a
// further nested subquery. ORDER BY keys are deterministic.
const QUERIES: &[&str] = &[
    // Nested scalar subquery in the folded subquery's WHERE.
    "SELECT g, (SELECT max(n) FROM t WHERE n < (SELECT avg(n) FROM t)) FROM t ORDER BY g, n",
    // Nested EXISTS inside the folded subquery's WHERE.
    "SELECT g, (SELECT count(*) FROM t WHERE EXISTS (SELECT 1 FROM t WHERE n > 50)) FROM t ORDER BY g, n",
    // Nested IN (SELECT …) inside the folded subquery's WHERE.
    "SELECT g, (SELECT sum(n) FROM t WHERE n IN (SELECT n FROM t WHERE n < 20)) FROM t ORDER BY g, n",
    // The folded subquery is an EXISTS whose body holds a nested scalar subquery.
    "SELECT g FROM t WHERE EXISTS (SELECT 1 FROM t WHERE n > (SELECT avg(n) FROM t)) ORDER BY g, n",
    // The folded subquery is an IN (SELECT …) whose body holds a nested subquery.
    "SELECT g, n FROM t WHERE n IN (SELECT n FROM t WHERE n >= (SELECT min(n) FROM t WHERE n > 5)) ORDER BY g, n",
    // Triple nesting: subquery within subquery within subquery, all self-contained.
    "SELECT g, (SELECT count(*) FROM t WHERE n < (SELECT max(n) FROM t WHERE n < (SELECT avg(n) FROM t))) FROM t ORDER BY g, n",
    // Nested subquery in arithmetic inside the folded subquery's projection.
    "SELECT g, (SELECT max(n) - (SELECT min(n) FROM t) FROM t) FROM t ORDER BY g, n",
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
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

#[test]
fn nested_subquery_folds_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the subquery folded.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn correlated_nested_subquery_falls_back() {
    let c = conn();
    // The innermost subquery references the OUTERMOST row (`x.g`), so the middle
    // subquery is correlated and must not be folded to a constant.
    let q = "SELECT x.g, (SELECT max(n) FROM t WHERE n < \
             (SELECT avg(n) FROM t t2 WHERE t2.g = x.g)) \
             FROM t x ORDER BY x.g, x.n";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    assert!(c.query(q).is_ok(), "tree-walker should run {q}");
}

#[test]
fn nested_subquery_matches_sqlite3() {
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
