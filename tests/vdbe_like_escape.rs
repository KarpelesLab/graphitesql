//! `text LIKE pattern ESCAPE c` desugars to the three-argument SQLite function
//! `like(pattern, text, c)`. The VDBE expression compiler previously accepted
//! only the two-argument `like`/`glob` function form, so any query using an
//! `ESCAPE` clause fell back to the tree-walker. The three-argument call is a
//! pure, context-free function (it reads only its argument values), so it now
//! compiles to `Op::Func` and runs on the VDBE — applying the escape character
//! and rejecting a non-single-character escape exactly as the tree-walker does.
//!
//! `query_vdbe` errors on any fallback to the tree-walker, so a passing query
//! proves the VDBE compiled the `ESCAPE` form. Results match the tree-walker and
//! the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

// `a` is a plain (unindexed) integer so `ORDER BY a` is served by the VDBE
// sorter rather than an index/rowid scan (the latter defers to the tree-walker).
const SETUP: &str = "\
    CREATE TABLE t(a INTEGER, s TEXT);\n\
    INSERT INTO t VALUES\n\
      (1,'100%'),(2,'1000'),(3,'a_b'),(4,'axb'),(5,'a\\b'),\n\
      (6,'25% off'),(7,'plain'),(8,NULL);\n";

const QUERIES: &[&str] = &[
    // `%` is a literal because `\` escapes it; only the row holding a literal `%`.
    "SELECT a, s FROM t WHERE s LIKE '100\\%' ESCAPE '\\' ORDER BY a",
    // Escaped `_` matches a literal underscore, not any character.
    "SELECT a, s FROM t WHERE s LIKE 'a\\_b' ESCAPE '\\' ORDER BY a",
    // Without escaping the `_`, it is a single-char wildcard (a_b and axb match).
    "SELECT a, s FROM t WHERE s LIKE 'a_b' ESCAPE '\\' ORDER BY a",
    // A non-backslash escape character.
    "SELECT a, s FROM t WHERE s LIKE '25x% off' ESCAPE 'x' ORDER BY a",
    // NOT LIKE … ESCAPE wraps the desugared call in NOT.
    "SELECT a, s FROM t WHERE s NOT LIKE '100\\%' ESCAPE '\\' ORDER BY a",
    // ESCAPE in the projection (a boolean column), not just the WHERE.
    "SELECT a, s LIKE '100\\%' ESCAPE '\\' AS m FROM t ORDER BY a",
    // The bare function form with three arguments.
    "SELECT a FROM t WHERE like('a\\_b', s, '\\') ORDER BY a",
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
fn like_escape_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the ESCAPE form compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn like_escape_invalid_errors_on_vdbe() {
    let c = conn();
    // A multi-character ESCAPE must error on the VDBE path just like the
    // tree-walker (SQLite: "ESCAPE expression must be a single character").
    let q = "SELECT s FROM t WHERE s LIKE '100\\%' ESCAPE 'xy'";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE error for {q}");
    assert!(c.query(q).is_err(), "expected tree-walker error for {q}");
}

#[test]
fn like_escape_matches_sqlite3() {
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
