//! `printf(fmt, ...)` (and its alias `format`) is pure string formatting over
//! the argument *values* — it reads no row or connection state — so it compiles
//! to `Op::Func` and runs on the VDBE rather than falling back to the
//! tree-walker. It routes through `func::eval_scalar` → `datetime::printf`,
//! giving exactly the tree-walker / sqlite3 result.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE
//! compiled the call. Results match the tree-walker and the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

// `a` is a plain (unindexed) integer so `ORDER BY a` is served by the VDBE
// sorter rather than an index/rowid scan (the latter defers to the tree-walker).
const SETUP: &str = "\
    CREATE TABLE t(a INTEGER, s TEXT, r REAL);\n\
    INSERT INTO t VALUES\n\
      (1,'one',1.5),(2,'two',2.25),(3,'three',-3.0),(4,NULL,4.0);\n";

const QUERIES: &[&str] = &[
    // Integer + string substitution.
    "SELECT a, printf('%d-%s', a, s) FROM t ORDER BY a",
    // The `format` alias.
    "SELECT a, format('[%5d]', a) FROM t ORDER BY a",
    // Width / precision on a float, plus a literal percent.
    "SELECT a, printf('%.2f%%', r) FROM t ORDER BY a",
    // Zero-padding and a hex conversion.
    "SELECT a, printf('%03d/%x', a, a) FROM t ORDER BY a",
    // Single-argument form (format string only, no conversions).
    "SELECT printf('plain') FROM t ORDER BY a",
    // A NULL argument substituted via %s.
    "SELECT a, printf('s=%s', s) FROM t ORDER BY a",
    // In a WHERE predicate.
    "SELECT a FROM t WHERE printf('%d', a) = '2' ORDER BY a",
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
fn printf_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the call compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn printf_matches_sqlite3() {
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
