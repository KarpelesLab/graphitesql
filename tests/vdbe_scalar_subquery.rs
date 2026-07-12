//! An uncorrelated, FROM-less scalar subquery `(SELECT <e> [WHERE <p>])` yields
//! its single column's value for the one rowless row, or NULL when a `WHERE`
//! predicate filters that row out. It now compiles inline on the VDBE instead of
//! deferring: the result register defaults to NULL, the optional predicate gates
//! it, and the projected value overwrites the NULL when the row qualifies.
//!
//! Subqueries that need machinery the const path lacks still defer to the
//! tree-walker: a real `FROM`, an aggregate or multi-column projection, or an
//! `ORDER BY`/`LIMIT`/`OFFSET`. Those remain correct via the fallback.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the call
//! compiled. Results are checked against the tree-walker and the sqlite3 3.50.4
//! CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

/// Scalar subqueries that now run on the VDBE.
const VDBE_QUERIES: &[&str] = &[
    "SELECT (SELECT 5)",
    "SELECT (SELECT 5) + 1",
    "SELECT (SELECT 'a' || 'b')",
    "SELECT (SELECT 5 WHERE 1)",     // predicate true → the value
    "SELECT (SELECT 5 WHERE 0)",     // predicate false → NULL
    "SELECT (SELECT 3 WHERE 1 > 2)", // predicate false → NULL
    "SELECT (SELECT 5 WHERE 0) IS NULL",
    "SELECT coalesce((SELECT 9 WHERE 0), 7)", // NULL subquery feeds coalesce
    "SELECT (SELECT 5) * (SELECT 2)",         // two subqueries in one expr
    "SELECT (SELECT NULL)",
    "SELECT (SELECT (SELECT 4))",         // nested scalar subquery
    "SELECT (SELECT abs(-3) WHERE 1)",    // a function inside the subquery
    "SELECT (SELECT 1) WHERE (SELECT 1)", // subquery in the outer WHERE too
];

/// Subqueries the const scalar-subquery arm does NOT inline (aggregate /
/// LIMIT / ORDER BY projections). Whether they run on some other VDBE path or
/// defer to the tree-walker, the rule is the same: the result must never be
/// wrong. (A different VDBE branch already handles some of these correctly, so
/// we assert "Ok ⇒ matches the tree-walker", not "always defers".)
const NEVER_WRONG_QUERIES: &[&str] = &[
    "SELECT (SELECT max(5))",       // aggregate projection
    "SELECT (SELECT 5 LIMIT 1)",    // explicit LIMIT
    "SELECT (SELECT 5 ORDER BY 1)", // ORDER BY (positional)
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
fn scalar_subquery_runs_on_vdbe_and_matches_tree_walker() {
    let c = Connection::open_memory().unwrap();
    for q in VDBE_QUERIES {
        // `query_vdbe` errors on fallback, so this proves the call compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn scalar_subquery_unsupported_shapes_are_never_wrong() {
    let c = Connection::open_memory().unwrap();
    for q in NEVER_WRONG_QUERIES {
        let want = c.query(q).expect("tree-walker failed");
        // The const arm doesn't inline these, but another VDBE path may handle
        // them — if so, the answer must match the tree-walker exactly.
        if let Ok(got) = c.query_vdbe(q) {
            assert_eq!(got.rows, want.rows, "VDBE answer wrong on {q}");
        }
    }
}

#[test]
fn scalar_subquery_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = Connection::open_memory().unwrap();
    for q in VDBE_QUERIES.iter().chain(NEVER_WRONG_QUERIES) {
        let got: Vec<Vec<String>> = c
            .query(q)
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
        // sqlite's `-ascii` mode *terminates* each row with 0x1e (and each field
        // with 0x1f), so an empty stdout is zero rows while a single `0x1e` is one
        // row whose only field is empty (a NULL). Strip the trailing terminator,
        // then split — never `filter` empties, or an all-NULL row vanishes.
        let want: Vec<Vec<String>> = if text.is_empty() {
            Vec::new()
        } else {
            text.strip_suffix('\u{1e}')
                .unwrap_or(&text)
                .split('\u{1e}')
                .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
                .collect()
        };
        assert_eq!(got, want, "graphite vs sqlite3 diverged on {q}");
    }
}
