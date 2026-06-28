//! A `FROM` reference naming an in-scope CTE now runs on the VDBE. The whole-query
//! `WITH` is materialized into the CTE environment before scanning, so the CTE's
//! rows are pulled straight from there (the CTE's name or alias is the row
//! qualifier; an explicit `WITH name(cols…)` list renames the body's columns). The
//! per-column affinity is resolved through the body — a constant/`VALUES` body
//! carries none, otherwise it flows through the single-source chain.
//!
//! A body that reads a *single-level* sibling CTE now runs too (see
//! `vdbe_cte_sibling.rs` for the full matrix); a two-or-more-level sibling chain,
//! or a recursive / join body, still defers — asserted separately. `query_vdbe`
//! errors on any fallback, so a passing query proves the VDBE handled the CTE
//! source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'x',10),(1,'y',20),\n\
      (2,'z',5),(2,'w',7),\n\
      (3,'q',100);\n";

// Each query references a CTE in its FROM clause. ORDER BY keys are deterministic
// so row order is stable for a direct comparison.
const QUERIES: &[&str] = &[
    // Plain projection over a single-table CTE.
    "WITH c AS (SELECT g, n FROM t) SELECT * FROM c ORDER BY g, n",
    // Aggregate/GROUP BY over the materialized CTE.
    "WITH c AS (SELECT g, n FROM t) SELECT g, sum(n) FROM c GROUP BY g ORDER BY g",
    // A constant VALUES body.
    "WITH c AS (VALUES (1),(2),(3)) SELECT column1 FROM c ORDER BY column1 DESC",
    // Explicit column-rename list applied to the body's columns.
    "WITH c(x,y) AS (SELECT g, n FROM t) SELECT x, y FROM c WHERE x > 1 ORDER BY y",
    // A WHERE inside the CTE body, ORDER BY in the outer query.
    "WITH c AS (SELECT g FROM t WHERE n > 5) SELECT * FROM c ORDER BY g",
    // The CTE reference carries its own alias.
    "WITH c AS (SELECT g, n FROM t) SELECT n FROM c AS c2 WHERE n > 5 ORDER BY n",
    // Outer projection computes over CTE columns.
    "WITH c AS (SELECT g, n FROM t) SELECT g, n * 2 FROM c ORDER BY g, n",
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
fn cte_source_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the CTE source compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn cte_referencing_sibling_runs_on_vdbe() {
    let c = conn();
    // The whole-query `WITH` is now materialized into scope before scanning, so a
    // CTE source whose body names a *single-level* sibling resolves from the
    // environment and runs on the VDBE (see `vdbe_cte_sibling.rs` for the full
    // matrix). A two-or-more-level sibling chain whose intermediate name can't be
    // resolved for origins still defers — correctly.
    let one = "WITH a AS (SELECT g FROM t), b AS (SELECT g FROM a) SELECT * FROM b";
    assert!(
        c.query_vdbe(one).is_ok(),
        "single-level sibling should run on the VDBE"
    );
    assert_eq!(c.query_vdbe(one).unwrap().rows, c.query(one).unwrap().rows);

    let two = "WITH a AS (SELECT g FROM t), b AS (SELECT g FROM a), d AS (SELECT g FROM b) \
               SELECT * FROM d";
    assert!(
        c.query_vdbe(two).is_err(),
        "deep sibling chain should defer"
    );
    assert!(c.query(two).is_ok(), "tree-walker should run {two}");
}

#[test]
fn cte_source_matches_sqlite3() {
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
