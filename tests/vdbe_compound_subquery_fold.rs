//! A non-correlated scalar / `EXISTS` / `IN (SELECT …)` subquery whose body is a
//! *compound* (`UNION` / `INTERSECT` / `EXCEPT`) now folds to a constant and runs
//! on the VDBE, where before any compound body made the fold bail. The fold proves
//! each compound arm self-contained against the same surrounding scope, and runs
//! the whole compound through the tree-walker — so set semantics, ordering, and
//! affinity/collation are evaluated exactly.
//!
//! For a value-producing fold (scalar / `IN`) every arm must project a *computed*
//! column: the whole compound's result column then carries NONE affinity, exactly
//! like an ordinary literal list, so folding the candidate set to literals is
//! affinity-exact. A *bare-column* arm would carry that column's affinity (which a
//! literal list lacks), so the router does not fold it to a literal list; instead
//! it runs on the VDBE through B5c-2's correlated-`IN (SELECT)` path, which
//! preserves the affinity exactly — see `bare_column_compound_arm_runs_via_correlated_in`.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE folded
//! the subquery. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'x',10),(1,'y',20),\n\
      (2,'z',5),(2,'w',7),\n\
      (3,'q',100);\n";

// Each query embeds a non-correlated subquery whose body is a compound; for the
// value-producing folds every arm projects a computed column.
const QUERIES: &[&str] = &[
    // IN over a FROM-less constant compound.
    "SELECT g, n FROM t WHERE n IN (SELECT 10 UNION SELECT 20) ORDER BY g, n",
    // IN over a table-backed compound, computed candidate in both arms.
    "SELECT g, n FROM t WHERE n IN (SELECT n+0 FROM t WHERE g=1 UNION SELECT n+0 FROM t WHERE g=2) ORDER BY g, n",
    // IN over an INTERSECT compound.
    "SELECT g, n FROM t WHERE n IN (SELECT n+0 FROM t INTERSECT SELECT 10) ORDER BY g, n",
    // IN over an EXCEPT compound.
    "SELECT g, n FROM t WHERE n IN (SELECT n+0 FROM t EXCEPT SELECT 5) ORDER BY g, n",
    // NOT IN over a compound.
    "SELECT g, n FROM t WHERE n NOT IN (SELECT 5 UNION SELECT 7) ORDER BY g, n",
    // Scalar subquery with a compound body (computed arms, ORDER BY/LIMIT picks one).
    "SELECT g, (SELECT 0 UNION SELECT 99 ORDER BY 1 DESC LIMIT 1) FROM t ORDER BY g, n",
    // EXISTS over a compound body.
    "SELECT g FROM t WHERE EXISTS (SELECT 1 FROM t WHERE n>50 UNION SELECT 1 FROM t WHERE n<0) ORDER BY g, n",
    // EXISTS over a compound whose arms are all empty -> false.
    "SELECT g FROM t WHERE NOT EXISTS (SELECT 1 FROM t WHERE n<0 UNION SELECT 1 FROM t WHERE n>999) ORDER BY g, n",
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
fn compound_subquery_folds_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the subquery folded.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn bare_column_compound_arm_runs_via_correlated_in() {
    let c = conn();
    // One arm projects a bare column (`a`), so the compound candidate carries that
    // column's affinity — folding to a plain literal list would change comparison
    // affinity, so the router does NOT fold it. It is no longer a VDBE fallback,
    // though: B5c-2's correlated-`IN (SELECT)` path wraps the whole predicate in a
    // FROM-less scalar select evaluated per outer row through the tree-walker, which
    // preserves the exact comparison affinity — so it runs on the VDBE with the
    // correct result (byte-identical to the tree-walker, verified against sqlite in
    // `compound_subquery_matches_sqlite3`'s sibling checks).
    let q = "SELECT g, n FROM t WHERE a IN (SELECT 'x' UNION SELECT a FROM t) ORDER BY g, n";
    let got = c
        .query_vdbe(q)
        .expect("runs on the VDBE via correlated IN")
        .rows;
    let want = c.query(q).unwrap().rows;
    assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
}

#[test]
fn compound_subquery_matches_sqlite3() {
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
