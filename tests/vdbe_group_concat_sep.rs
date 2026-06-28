//! Two-argument `group_concat(x, sep)` and its standard-SQL alias
//! `string_agg(x, sep)` now run on the VDBE. The separator is a constant (the
//! tree-walker evaluates it in a rowless context), so the VDBE captures the
//! literal separator at compile time and threads it through the aggregate
//! accumulator — joining with it instead of the default `,`.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE
//! compiled the two-argument form. Results match the tree-walker and sqlite3
//! 3.50.4. A `DISTINCT` two-argument call and a non-constant separator are left
//! to fall back (the tree-walker handles them), which `query_vdbe` reports as an
//! error — asserted separately.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

// Plain (unindexed) columns so `ORDER BY` is served by the VDBE sorter rather
// than an index/rowid scan (the latter defers the whole query to the tree-walker).
const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, s TEXT);\n\
    INSERT INTO t VALUES\n\
      (1,'a'),(1,'b'),(1,'c'),(2,'x'),(2,'y'),(3,'solo'),(3,NULL);\n";

const QUERIES: &[&str] = &[
    // Bare aggregate with an explicit separator.
    "SELECT group_concat(s, '-') FROM t",
    // The `string_agg` alias (separator mandatory).
    "SELECT string_agg(s, ', ') FROM t",
    // A multi-character separator.
    "SELECT group_concat(s, ' :: ') FROM t",
    // Grouped: one concatenation per group, custom separator.
    "SELECT g, group_concat(s, '|') FROM t GROUP BY g ORDER BY g",
    // Grouped via the alias.
    "SELECT g, string_agg(s, '/') FROM t GROUP BY g ORDER BY g",
    // Default separator still works (one-argument form, unchanged).
    "SELECT group_concat(s) FROM t",
    // Ordered group_concat with a custom separator.
    "SELECT group_concat(s, '-' ORDER BY s DESC) FROM t WHERE g = 1",
    // An empty-string separator.
    "SELECT group_concat(s, '') FROM t WHERE g = 2",
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
fn group_concat_sep_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the call compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn group_concat_sep_distinct_and_dynamic_fall_back() {
    let c = conn();
    // DISTINCT with a second (separator) argument is a tree-walker error, so the
    // VDBE must decline to compile it (fall back) rather than silently use `,`.
    let distinct = "SELECT group_concat(DISTINCT s, '-') FROM t";
    assert!(
        c.query_vdbe(distinct).is_err(),
        "expected VDBE fallback/error for {distinct}"
    );
    assert!(
        c.query(distinct).is_err(),
        "tree-walker should reject {distinct}"
    );
    // A non-constant (column) separator must fall back to the tree-walker, which
    // evaluates it rowlessly; the VDBE only accepts a literal separator.
    let dynamic = "SELECT group_concat(s, s) FROM t";
    assert!(
        c.query_vdbe(dynamic).is_err(),
        "expected VDBE fallback for non-constant separator {dynamic}"
    );
}

#[test]
fn group_concat_sep_matches_sqlite3() {
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
