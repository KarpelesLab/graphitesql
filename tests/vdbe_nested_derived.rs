//! A `FROM` subquery whose own source is another derived table (a nested
//! subquery, to any depth) now runs on the VDBE. Each output column's
//! `(affinity, collation)` is resolved through the chain of single-source
//! derived tables, so an inherited affinity — e.g. an `INTEGER` column read
//! through two `SELECT *` wrappers — keeps coercing exactly as SQLite does.
//!
//! A body that is a NATURAL/USING join, a compound, a view, a CTE, or a
//! table-valued function still defers to the tree-walker (`subquery_column_origins`
//! returns `None`), as does any derived column with a non-BINARY collation. (A
//! *plain* join body now runs — see `vdbe_derived_join.rs`.)
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE ran
//! the nested source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'x',10),(1,'y',20),\n\
      (2,'z',5),(2,'w',7),\n\
      (3,'q',100);\n";

// Each query's FROM source is a subquery over another subquery. ORDER BY keys
// are deterministic so the row order is stable for a direct comparison.
const QUERIES: &[&str] = &[
    // Two levels, projection + rename + inner WHERE.
    "SELECT x FROM (SELECT g AS x FROM (SELECT g FROM t WHERE n > 5)) ORDER BY x",
    // Two levels of wildcard, outer WHERE.
    "SELECT * FROM (SELECT * FROM (SELECT n, a FROM t)) WHERE n > 5 ORDER BY n",
    // INTEGER affinity must flow through two levels: '10' coerces to 10.
    "SELECT n FROM (SELECT n FROM (SELECT n FROM t)) WHERE n = '10'",
    // Three levels deep.
    "SELECT a FROM (SELECT a FROM (SELECT a FROM (SELECT a FROM t WHERE g = 1))) ORDER BY a",
    // A computed column inside the nested derived, then an outer filter on it.
    "SELECT y FROM (SELECT n * 2 AS y FROM (SELECT n FROM t)) WHERE y > 15 ORDER BY y",
    // An aggregate in the outer query over a nested derived source.
    "SELECT count(*), sum(n) FROM (SELECT n FROM (SELECT n FROM t WHERE n < 50))",
    // GROUP BY in the outer query over a nested derived source.
    "SELECT g, sum(n) FROM (SELECT g, n FROM (SELECT g, n FROM t)) GROUP BY g ORDER BY g",
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
fn nested_derived_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the nested source compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn nested_derived_over_a_natural_join_falls_back() {
    let c = conn();
    // The inner derived body is a NATURAL join, whose coalesced shared column
    // `subquery_column_origins` can't resolve by a bare-name lookup — the VDBE
    // declines and the tree-walker handles the query. (A *plain* join body runs;
    // see `vdbe_derived_join.rs`.)
    let q = "SELECT x FROM (SELECT g AS x FROM t t1 NATURAL JOIN t t2) ORDER BY x";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    assert!(c.query(q).is_ok(), "tree-walker should run {q}");
}

#[test]
fn nested_derived_matches_sqlite3() {
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
