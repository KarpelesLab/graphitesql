//! A `FROM` reference to a CTE whose body reads a *sibling* CTE now runs on the
//! VDBE. The whole-query `WITH` is materialized into the CTE environment at the
//! top of the VDBE select path, so a CTE source's rows are pulled straight from
//! there (correct even when the body reads a sibling, is recursive, or shadows a
//! base-table name — the tree-walker resolves all of that during materialization).
//! The per-column affinity is resolved through the CTE body with the sibling CTEs
//! in scope (`subquery_column_origins_in`), so an `INTEGER` column read through a
//! sibling keeps coercing exactly as SQLite does.
//!
//! A recursive (compound) body, a join body, or a two-or-more-level sibling chain
//! whose intermediate names can't be resolved still defers to the tree-walker
//! (the body origins don't resolve) — correct, never wrong.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE ran the
//! CTE source itself. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'x',10),(1,'y',20),\n\
      (2,'z',5),(2,'w',7),\n\
      (3,'q',100);\n";

// Each query reaches a CTE whose body reads a sibling CTE (or is otherwise pulled
// from the materialized environment). ORDER BY pins the row order.
const QUERIES: &[&str] = &[
    // Single-level sibling reference.
    "WITH a AS (SELECT g,n FROM t), b AS (SELECT g FROM a) SELECT * FROM b ORDER BY 1",
    // Affinity-sensitive outer comparison: `g` keeps INTEGER affinity through the
    // sibling, so `g = '2'` coerces the text and matches.
    "WITH a AS (SELECT g,n FROM t), b AS (SELECT g FROM a) \
     SELECT g FROM b WHERE g = '2' ORDER BY 1",
    // Sibling with an explicit `WITH name(cols…)` rename.
    "WITH a AS (SELECT g,n FROM t), b(k) AS (SELECT g FROM a) SELECT k FROM b ORDER BY 1",
    // Grouped aggregate over a sibling CTE.
    "WITH a AS (SELECT g,n FROM t), b AS (SELECT g,n FROM a) \
     SELECT g, sum(n) FROM b GROUP BY g ORDER BY 1",
    // A CTE that shadows the base-table name `t` (the CTE wins).
    "WITH t AS (SELECT 9 AS g) SELECT g FROM t",
    // A CTE joined with a base table.
    "WITH c AS (SELECT g,n FROM t) SELECT t.g, c.n FROM t JOIN c ON t.g=c.g ORDER BY 1,2",
    // A constant / `VALUES` CTE body still works (no affinity, BINARY).
    "WITH v(a,b) AS (VALUES (1,2),(3,4)) SELECT a+b FROM v ORDER BY 1",
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
fn sibling_cte_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE scanned the CTE
        // source (incl. the sibling reference) itself.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn deep_or_compound_cte_bodies_fall_back() {
    let c = conn();
    // A recursive (compound) body, a join body, and a two-level sibling chain whose
    // intermediate name can't be resolved for origins all defer to the tree-walker —
    // correctly (the tree-walker still runs them).
    for q in [
        "WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<3) \
         SELECT i FROM r ORDER BY 1",
        "WITH j AS (SELECT t.g FROM t JOIN t t2 ON t.g=t2.g) SELECT count(*) FROM j",
        "WITH a AS (SELECT g FROM t), b AS (SELECT g FROM a), d AS (SELECT g FROM b) \
         SELECT count(*) FROM d",
    ] {
        assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
        assert!(c.query(q).is_ok(), "tree-walker should run {q}");
    }
}

#[test]
fn sibling_cte_matches_sqlite3() {
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
