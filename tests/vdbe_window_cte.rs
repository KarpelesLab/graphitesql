//! A window function whose `FROM` source names a whole-query `WITH` CTE now runs
//! on the VDBE. The CTE is resolved from `sel.ctes` (the same way the non-window
//! scan path resolves a CTE FROM-source): its columns' `(affinity, collation)`
//! flow through the CTE body — an explicit `WITH name(cols…)` rename and any
//! inherited affinity are honored — and the base scan materializes the CTE's rows
//! through the derived-source path, so columns and rows stay in lockstep. A query
//! that merely carries an unused `WITH` over a base table runs too.
//!
//! A constant / `VALUES` CTE body is supported too (its columns carry no affinity
//! and BINARY collation). A CTE body the source path still can't resolve (a
//! non-constant compound / join / view body), a window/projection that references
//! the CTE's non-existent rowid, or a *join* that carries CTEs (its column set is
//! resolved statically and can't see a CTE binding) all still defer.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE ran the
//! window over the CTE source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'x',10),(1,'y',20),\n\
      (2,'z',5),(2,'w',7),\n\
      (3,'q',100);\n";

// Each query runs a window function over a CTE FROM-source (or carries an unused
// WITH). ORDER BY keys are deterministic so the row order is stable.
const QUERIES: &[&str] = &[
    // PARTITION-only aggregate window over a one-CTE source.
    "WITH cte AS (SELECT g, n FROM t) SELECT g, sum(n) OVER (PARTITION BY g) FROM cte ORDER BY g, n",
    // ORDER-only ranking window over a CTE source.
    "WITH cte AS (SELECT a, n FROM t) SELECT a, row_number() OVER (ORDER BY n) FROM cte ORDER BY n",
    // An unused whole-query WITH over a plain base table still runs.
    "WITH cte AS (SELECT 1) SELECT g, sum(n) OVER (PARTITION BY g) FROM t ORDER BY g, n",
    // Explicit WITH name(cols…) rename, referenced in the window and projection.
    "WITH cte(grp, val) AS (SELECT g, n FROM t) SELECT grp, sum(val) OVER (PARTITION BY grp) FROM cte ORDER BY grp, val",
    // The CTE body is itself a nested derived table.
    "WITH cte AS (SELECT g, n FROM (SELECT g, n FROM t WHERE n > 5)) SELECT g, rank() OVER (PARTITION BY g ORDER BY n) FROM cte ORDER BY g, n",
    // INTEGER affinity must flow through the CTE: '10' coerces to 10.
    "WITH cte AS (SELECT n FROM t) SELECT n, sum(n) OVER () FROM cte WHERE n = '10'",
    // Two CTEs, the window over the second.
    "WITH a AS (SELECT g FROM t), b AS (SELECT g, n FROM t) SELECT g, lag(n) OVER (ORDER BY n) FROM b ORDER BY n",
    // A constant `VALUES` CTE body — no affinity, BINARY collation.
    "WITH cte AS (VALUES (1,10),(1,20),(2,5)) SELECT column1, sum(column2) OVER (PARTITION BY column1) FROM cte ORDER BY column1, column2",
    // A `VALUES` CTE with an explicit `WITH name(cols…)` rename.
    "WITH cte(p, q) AS (VALUES (1,10),(2,20),(2,5)) SELECT p, sum(q) OVER (PARTITION BY p) FROM cte ORDER BY p, q",
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
fn window_over_cte_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the window compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn window_over_join_with_cte_falls_back() {
    let c = conn();
    // A join that carries a CTE: the join column set is resolved statically and
    // can't see the CTE binding, so the window defers even though the CTE is
    // unused by the join sources.
    let q = "WITH cte AS (SELECT 1) \
             SELECT t1.g, row_number() OVER (ORDER BY t1.n) \
             FROM t t1 JOIN t t2 ON t1.g = t2.g ORDER BY t1.n";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    assert!(c.query(q).is_ok(), "tree-walker should run {q}");
}

#[test]
fn window_over_cte_matches_sqlite3() {
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
