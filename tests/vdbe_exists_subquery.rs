//! `[NOT] EXISTS (SELECT <e> [WHERE <p>])` over a FROM-less body tests whether
//! the inner's single rowless row survives: with no `WHERE` (or a true one) the
//! row exists, so `EXISTS` is 1; a false `WHERE` drops it, so `EXISTS` is 0
//! (inverted for `NOT EXISTS`). It now compiles inline on the VDBE — a constant
//! is loaded and an optional predicate flips it — instead of deferring.
//!
//! `EXISTS` never evaluates the projection, so a multi-column inner is fine, but
//! each term is still compiled to force resolution: an unresolved column or a
//! `SELECT *` (no tables) defers and the tree-walker rejects it. An aggregate
//! projection (which yields a row even over a false-`WHERE` empty input, making
//! `EXISTS` always 1), a real `FROM`, or an `ORDER BY`/`LIMIT`/`OFFSET` body
//! defers and stays correct via the fallback.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the call
//! compiled. Results are checked against the tree-walker and the sqlite3 3.50.4
//! CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

/// EXISTS subqueries that now run on the VDBE.
const VDBE_QUERIES: &[&str] = &[
    "SELECT EXISTS(SELECT 1)",               // no predicate → exists
    "SELECT EXISTS(SELECT 5 WHERE 1)",       // true predicate → exists
    "SELECT EXISTS(SELECT 5 WHERE 0)",       // false predicate → not exists
    "SELECT EXISTS(SELECT 1 WHERE 1>2)",     // false predicate → not exists
    "SELECT NOT EXISTS(SELECT 1)",           // negated, exists → 0
    "SELECT NOT EXISTS(SELECT 1 WHERE 0)",   // negated, not exists → 1
    "SELECT EXISTS(SELECT 1, 2)",            // multi-column is legal for EXISTS
    "SELECT EXISTS(SELECT 1, 2 WHERE 0)",    // multi-column, filtered out
    "SELECT EXISTS(SELECT abs(-3) WHERE 1)", // a function inside the body
    "SELECT EXISTS(SELECT 1) AND EXISTS(SELECT 2 WHERE 0)", // two of them
    "SELECT EXISTS(SELECT 1) WHERE EXISTS(SELECT 1)", // in the outer WHERE
    "SELECT CASE WHEN EXISTS(SELECT 1 WHERE 0) THEN 'y' ELSE 'n' END",
];

/// Bodies the EXISTS arm does NOT inline (aggregate / LIMIT / ORDER BY). Whether
/// they run on some other VDBE path or defer, the answer must never be wrong.
const NEVER_WRONG_QUERIES: &[&str] = &[
    "SELECT EXISTS(SELECT max(5))", // aggregate → row exists even if empty
    "SELECT EXISTS(SELECT max(5) WHERE 0)", // aggregate over zero rows → still 1
    "SELECT EXISTS(SELECT 5 LIMIT 1)", // explicit LIMIT
    "SELECT EXISTS(SELECT 5 ORDER BY 1)", // ORDER BY (positional)
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
fn exists_subquery_runs_on_vdbe_and_matches_tree_walker() {
    let c = Connection::open_memory().unwrap();
    for q in VDBE_QUERIES {
        // `query_vdbe` errors on fallback, so this proves the call compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn exists_subquery_unsupported_shapes_are_never_wrong() {
    let c = Connection::open_memory().unwrap();
    for q in NEVER_WRONG_QUERIES {
        let want = c.query(q).expect("tree-walker failed");
        if let Ok(got) = c.query_vdbe(q) {
            assert_eq!(got.rows, want.rows, "VDBE answer wrong on {q}");
        }
    }
}

#[test]
fn exists_subquery_matches_sqlite3() {
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
        // with 0x1f), so empty stdout is zero rows while a single `0x1e` is one
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
