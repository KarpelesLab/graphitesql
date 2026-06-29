//! A `FROM`-less `SELECT` yields at most one row, so any `ORDER BY` whose terms
//! resolve is a no-op — there is nothing to sort — and now runs on the VDBE
//! instead of deferring. The order key is still compiled (so it must type-check
//! and its column/alias refs must resolve), then discarded. A *positional*
//! ordinal (`ORDER BY 2`) needs range validation against the projection, so it
//! still defers to the tree-walker, as does any unresolved column reference.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the call
//! compiled. Results are checked against the tree-walker and the sqlite3 3.50.4
//! CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

/// Cases that now run on the VDBE: the order key resolves and is a no-op.
const VDBE_QUERIES: &[&str] = &[
    "SELECT 7 ORDER BY abs(-1)",
    "SELECT 7 ORDER BY 1 + 1", // an expression, not a bare ordinal
    "SELECT 7 ORDER BY 'x'",   // a constant string key
    "SELECT 9 ORDER BY abs(-1) DESC",
    "SELECT 5 WHERE 1 ORDER BY 2 + 3",
    "SELECT 5 WHERE 0 ORDER BY 1 + 1", // predicate drops the row first
    "SELECT 3 ORDER BY 1 + 1 LIMIT 1",
    "SELECT DISTINCT 4 ORDER BY 'k'",
    "SELECT 1, 'a', 2.5 ORDER BY abs(-2), 1 + 0", // multi-key, both no-op exprs
];

/// Cases that still defer (positional ordinal or unresolved ref). They must
/// agree with the tree-walker and sqlite, but must NOT run on the VDBE.
const DEFER_QUERIES: &[&str] = &[
    "SELECT 1 ORDER BY 1",      // positional ordinal
    "SELECT 1, 2 ORDER BY 2",   // positional ordinal in range
    "SELECT 1 ORDER BY 1 DESC", // positional with direction
    "SELECT 1 AS a ORDER BY a", // output-alias ref (unresolved in VDBE scope)
];

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
fn fromless_order_runs_on_vdbe_and_matches_tree_walker() {
    let c = Connection::open_memory().unwrap();
    for q in VDBE_QUERIES {
        // `query_vdbe` errors on fallback, so this proves the call compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn fromless_order_positional_still_defers() {
    let c = Connection::open_memory().unwrap();
    for q in DEFER_QUERIES {
        // The positional/unresolved cases must NOT compile on the VDBE...
        assert!(
            c.query_vdbe(q).is_err(),
            "expected VDBE fallback (defer) on {q}",
        );
        // ...but the tree-walker still answers them correctly.
        assert!(c.query(q).is_ok(), "tree-walker failed on {q}");
    }
}

#[test]
fn fromless_order_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = Connection::open_memory().unwrap();
    for q in VDBE_QUERIES.iter().chain(DEFER_QUERIES) {
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
        let want: Vec<Vec<String>> = text
            .split('\u{1e}')
            .filter(|r| !r.is_empty())
            .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
            .collect();
        assert_eq!(got, want, "graphite vs sqlite3 diverged on {q}");
    }
}
