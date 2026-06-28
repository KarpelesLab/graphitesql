//! A window function whose `FROM` source is a derived subquery (to any depth)
//! now runs on the VDBE. The derived source's output columns are resolved
//! through `subquery_column_origins`, so each column keeps its inherited
//! `(affinity, collation)` while the window operator partitions/orders/frames
//! over the materialized rows.
//!
//! A constant / `VALUES` derived body is also supported — its columns carry no
//! affinity and BINARY collation, exactly as the non-window derived scan
//! materializes them. A derived body that is a join, a non-constant compound, a
//! view, or a table-valued function still defers to the tree-walker, as does any
//! query whose window or projection references the (non-existent) rowid of the
//! derived source.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE ran
//! the window over the derived source. Checked against the tree-walker and
//! sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'x',10),(1,'y',20),\n\
      (2,'z',5),(2,'w',7),\n\
      (3,'q',100);\n";

// Each query runs a window function over a derived subquery source. ORDER BY
// keys are deterministic so the row order is stable for a direct comparison.
const QUERIES: &[&str] = &[
    // PARTITION-only aggregate window over a one-level derived source.
    "SELECT g, sum(n) OVER (PARTITION BY g) FROM (SELECT g, n FROM t) ORDER BY g, n",
    // ORDER-only ranking window.
    "SELECT a, row_number() OVER (ORDER BY n) FROM (SELECT a, n FROM t) ORDER BY n",
    // PARTITION + ORDER, with a WHERE inside the derived source.
    "SELECT g, n, rank() OVER (PARTITION BY g ORDER BY n DESC) FROM (SELECT g, n FROM t WHERE n > 5) ORDER BY g, n",
    // Empty frame over a computed column in the derived source.
    "SELECT y, sum(y) OVER () FROM (SELECT n * 2 AS y FROM t) ORDER BY y",
    // A two-level-nested derived source under a lag() window.
    "SELECT g, lag(n) OVER (ORDER BY n) FROM (SELECT g, n FROM (SELECT g, n FROM t)) ORDER BY n",
    // count(*) OVER a partition while the projection reads a different column.
    "SELECT a, count(*) OVER (PARTITION BY g) FROM (SELECT g, a FROM t) ORDER BY g, a",
    // A derived alias, referenced qualified in both the window and ORDER BY.
    "SELECT v.g, sum(v.n) OVER (PARTITION BY v.g) FROM (SELECT g, n FROM t) v ORDER BY v.g, v.n",
    // A constant `VALUES` derived body — no affinity, BINARY collation.
    "SELECT column1, sum(column2) OVER (PARTITION BY column1) FROM (VALUES (1,10),(1,20),(2,5)) ORDER BY column1, column2",
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
fn window_over_derived_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the window compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn window_over_derived_join_falls_back() {
    let c = conn();
    // The derived body is a join, which `subquery_column_origins` can't resolve —
    // the VDBE declines and the tree-walker handles the window.
    let q = "SELECT x, row_number() OVER (ORDER BY x) \
             FROM (SELECT t1.g x FROM t t1 JOIN t t2 ON t1.g = t2.g)";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    assert!(c.query(q).is_ok(), "tree-walker should run {q}");
}

#[test]
fn window_over_derived_matches_sqlite3() {
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
