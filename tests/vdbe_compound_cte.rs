//! A compound SELECT (`UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT`) whose
//! whole-query `WITH` is referenced by one or more arms now runs on the VDBE.
//!
//! The outer CTEs are materialized into the CTE environment (so the first core's
//! output-collation scan resolves them) and threaded into every operand (so each
//! arm materializes them through the derived-source path). A CTE that can't run
//! on the VDBE — a sibling/self reference, a recursive body — makes the owning
//! arm decline, falling the whole compound back to the tree-walker.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE ran
//! the compound. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'x',10),(1,'y',20),\n\
      (2,'z',5),(2,'w',7),\n\
      (3,'q',100);\n";

// Each compound references the whole-query WITH from at least one arm. ORDER BY
// keys are deterministic so the row order is stable for a direct comparison.
const QUERIES: &[&str] = &[
    // Two CTEs, each its own arm, UNION ALL (keeps duplicates).
    "WITH a AS (SELECT g, n FROM t), b AS (SELECT g, n FROM t WHERE n > 50) \
     SELECT * FROM a UNION ALL SELECT * FROM b ORDER BY g, n",
    // CTE arm UNION a base-table arm (dedups + sorts).
    "WITH a AS (SELECT g FROM t) SELECT * FROM a UNION SELECT g FROM t WHERE g > 1 ORDER BY g",
    // Base-table arm INTERSECT a CTE arm.
    "WITH a AS (SELECT g FROM t) SELECT g FROM t INTERSECT SELECT * FROM a ORDER BY g",
    // A constant VALUES-body CTE arm UNION a base-table arm.
    "WITH a AS (VALUES (1),(2),(3)) SELECT column1 FROM a UNION SELECT g FROM t ORDER BY 1",
    // Base-table arm EXCEPT a filtered CTE arm (empty result).
    "WITH a AS (SELECT g FROM t WHERE n > 5) SELECT g FROM t EXCEPT SELECT * FROM a ORDER BY g",
    // The same CTE referenced by both arms.
    "WITH a AS (SELECT g, n FROM t) SELECT * FROM a UNION ALL SELECT * FROM a ORDER BY g, n",
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
fn compound_cte_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the compound compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn compound_with_recursive_cte_falls_back() {
    let c = conn();
    // A recursive CTE body can't run on the VDBE; the owning arm declines and
    // the tree-walker handles the whole compound.
    let q = "WITH RECURSIVE r(k) AS (SELECT 1 UNION ALL SELECT k+1 FROM r WHERE k < 3) \
             SELECT k FROM r UNION SELECT g FROM t";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    assert!(c.query(q).is_ok(), "tree-walker should run {q}");
}

#[test]
fn compound_cte_matches_sqlite3() {
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
