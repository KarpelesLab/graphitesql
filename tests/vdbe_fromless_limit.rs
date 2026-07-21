//! A `FROM`-less `SELECT` yields at most one row, so `LIMIT`/`OFFSET` just emit or
//! suppress that single row and `DISTINCT` is a no-op — all three run on the VDBE.
//! The bounds are compiled into registers (constant OR not — a `(SELECT …)` /
//! string / function bound, plus `Op::MustBeInt` for SQLite's coercion), then the
//! row is skipped when `LIMIT` is exactly 0 or a positive `OFFSET` skips past it; a
//! negative `LIMIT` is unlimited and a non-positive `OFFSET` skips nothing, matching
//! SQLite. (`ORDER BY` over a FROM-less select still defers — its term validation
//! is left to the tree-walker.)
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the call
//! compiled. Results are checked against the tree-walker and the sqlite3 3.50.4
//! CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const QUERIES: &[&str] = &[
    // LIMIT keeps or drops the single row.
    "SELECT 5 LIMIT 1",
    "SELECT 5 LIMIT 0",
    "SELECT 5 LIMIT -1", // negative = unlimited → kept
    "SELECT 5 LIMIT 99",
    // OFFSET past the single row drops it; OFFSET 0 keeps it.
    "SELECT 5 LIMIT 2 OFFSET 1",
    "SELECT 5 LIMIT 1 OFFSET 0",
    "SELECT 5 LIMIT 1 OFFSET -1", // negative offset = no skip → kept
    // Constant LIMIT/OFFSET *expressions* fold like the scan path.
    "SELECT 9 LIMIT 1 + 1 OFFSET 0",
    "SELECT 3 LIMIT CAST('1' AS INT)",
    "SELECT 8 LIMIT 1 OFFSET 3 - 3",
    // Genuinely non-constant bounds now compile into registers + MustBeInt.
    "SELECT 5 LIMIT (SELECT 2)",
    "SELECT 5 LIMIT '1'",
    "SELECT 5 LIMIT abs(-1)",
    "SELECT 5 LIMIT 1 OFFSET (SELECT 1)",
    // DISTINCT is a no-op on a one-row result.
    "SELECT DISTINCT 1",
    "SELECT DISTINCT 1, 2, 3",
    "SELECT DISTINCT 'hello'",
    // Combined with WHERE (the predicate gates before the bound applies).
    "SELECT 7 WHERE 1 LIMIT 1",
    "SELECT 7 WHERE 0 LIMIT 1",
    "SELECT DISTINCT 4 WHERE 2 > 1",
    // Multi-column projection with a bound.
    "SELECT 1, 'a', 2.5 LIMIT 1",
];

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
fn fromless_limit_runs_on_vdbe_and_matches_tree_walker() {
    let c = Connection::open_memory().unwrap();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the call compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn fromless_non_integer_limit_is_datatype_mismatch_on_vdbe() {
    use graphitesql::Error;
    let c = Connection::open_memory().unwrap();
    for q in [
        "SELECT 1 LIMIT 2.9",
        "SELECT 1 LIMIT NULL",
        "SELECT 1 LIMIT 'x'",
        "SELECT 1 LIMIT 1 OFFSET NULL",
    ] {
        match c.query_vdbe(q).unwrap_err() {
            Error::Error(m) => assert_eq!(m, "datatype mismatch", "for `{q}`"),
            other => panic!("`{q}`: expected datatype mismatch, got {other:?}"),
        }
    }
}

#[test]
fn fromless_limit_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = Connection::open_memory().unwrap();
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
            .arg(format!("{q};"))
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
