//! `SELECT DISTINCT … GROUP BY …` now runs on the VDBE. Grouping produces one
//! row per group; `DISTINCT` then dedups those output rows. The grouped compiler
//! used to bail outright on `DISTINCT`; it now forces the binding-driven general
//! path and inserts a `DistinctCheck` after `HAVING` and before `OFFSET`/`LIMIT`
//! and the sorter — so dedup precedes ordering and the row counters, matching
//! SQLite (dedup-then-sort-then-limit).
//!
//! Dedup compares output rows under BINARY. The table-wide collation guard already
//! defers a non-BINARY declared column; an explicit `COLLATE` on an otherwise-
//! BINARY output column is caught by a dedicated guard (see
//! `distinct_collation_output_falls_back`).
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE ran the
//! grouped DISTINCT itself. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'x',10),(1,'y',20),\n\
      (2,'z',5),(2,'w',7),\n\
      (3,'q',100);\n";

// Each query groups, then dedups the grouped output. The grouped counts/keys
// collide across groups so `DISTINCT` actually removes rows.
const QUERIES: &[&str] = &[
    // Aggregate output that repeats across groups (counts 2,2,1 -> 1,2).
    "SELECT DISTINCT count(*) FROM t GROUP BY g ORDER BY 1",
    // Computed key collapsing two source values into one group, then deduped.
    "SELECT DISTINCT g/2 FROM t GROUP BY g/2 ORDER BY 1",
    // No ORDER BY: group emission is key-sorted, dedup keeps the first (2, then 1).
    "SELECT DISTINCT count(*) FROM t GROUP BY g",
    // Multi-column distinct row.
    "SELECT DISTINCT g%2, count(*) FROM t GROUP BY g ORDER BY 1, 2",
    // DISTINCT then LIMIT (dedup precedes the counter): maxes 20,7,100 -> 7,20.
    "SELECT DISTINCT max(n) FROM t GROUP BY g ORDER BY 1 LIMIT 2",
    // DISTINCT with HAVING.
    "SELECT DISTINCT count(*) FROM t GROUP BY g HAVING count(*) >= 1 ORDER BY 1",
    // DISTINCT over a computed key with an aggregate alongside.
    "SELECT DISTINCT n%2, count(*) FROM t GROUP BY n%2 ORDER BY 1",
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
fn grouped_distinct_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE deduped the
        // grouped output itself.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn distinct_collation_output_falls_back() {
    let c = conn();
    // An explicit `COLLATE NOCASE` on a DISTINCT output column would dedup
    // case-insensitively; the VDBE's `DistinctCheck` is BINARY, so it defers.
    let q = "SELECT DISTINCT a COLLATE NOCASE FROM t GROUP BY g ORDER BY 1";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
    assert!(c.query(q).is_ok(), "tree-walker should run {q}");
}

#[test]
fn grouped_distinct_matches_sqlite3() {
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
