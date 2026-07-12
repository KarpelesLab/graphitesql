//! A second batch of pure scalar functions promoted onto the VDBE via
//! `Op::Func`: the inverse-hyperbolic math (`asinh`/`acosh`/`atanh`), the
//! Unicode-escape helpers (`unistr`/`unistr_quote`), the JSON syntax probe
//! (`json_error_position`), and the build-constant identifiers
//! (`sqlite_version`/`sqlite_source_id`). Each dispatches to `func::eval_scalar`
//! over its reconstructed argument *values* with no `ctx` access, so the VDBE
//! reproduces the tree-walker byte-for-byte.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the call
//! compiled to `Op::Func` rather than deferring. The value-producing queries are
//! also checked against the sqlite3 3.50.4 CLI; the two build-constant functions
//! are exercised only for VDBE/tree-walker parity (their text is graphite's own).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

// `a` is a plain (unindexed) integer so `ORDER BY a` uses the VDBE sorter.
const SETUP: &str = "\
    CREATE TABLE t(a INTEGER, x REAL, s TEXT);\n\
    INSERT INTO t VALUES\n\
      (1, 2.0, '{\"a\":1}'),\n\
      (2, 0.5, '{\"a\":}'),\n\
      (3, 1.0, 'not json'),\n\
      (4, NULL, NULL);\n";

// Queries whose results are byte-comparable against sqlite3.
const SHARED: &[&str] = &[
    // Inverse hyperbolic over a stored REAL (NULL passes through).
    "SELECT a, asinh(x) FROM t ORDER BY a",
    "SELECT a, atanh(x) FROM t ORDER BY a",
    // acosh is real only for x >= 1; 0.5 yields the SQLite out-of-domain result.
    "SELECT a, acosh(x) FROM t ORDER BY a",
    // unistr() decodes \uXXXX escapes; a constant exercises the value path.
    "SELECT a, unistr('z\\u0061p') FROM t ORDER BY a",
    // unistr_quote() of a control character renders as unistr('\\uXXXX…').
    "SELECT a, unistr_quote(char(9) || s) FROM t WHERE s IS NOT NULL ORDER BY a",
    // json_error_position() over stored text: 0 when valid, 1-based offset else.
    "SELECT a, json_error_position(s) FROM t ORDER BY a",
    // In a WHERE predicate.
    "SELECT a FROM t WHERE json_error_position(s) = 0 ORDER BY a",
    // Arithmetic over the math result.
    "SELECT a, asinh(x) + 1 FROM t ORDER BY a",
];

// Build-constant functions: VDBE/tree-walker parity only (graphite's own ids).
const LOCAL_ONLY: &[&str] = &[
    "SELECT length(sqlite_version()) > 0",
    "SELECT typeof(sqlite_source_id())",
    "SELECT sqlite_version() = sqlite_version()",
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
fn extra_scalar_fns_run_on_vdbe_and_match_tree_walker() {
    let c = conn();
    for q in SHARED.iter().chain(LOCAL_ONLY) {
        // `query_vdbe` errors on fallback, so this proves the call compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn extra_scalar_fns_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    for q in SHARED {
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
