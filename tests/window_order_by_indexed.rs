//! Regression: a plain window function (`f(x) OVER …`) combined with an outer
//! `ORDER BY`, over a base table that has a usable index, must still apply the
//! `ORDER BY` to the output. The window rewrite replaces each `OVER` call with a
//! synthetic `__winN` column, after which the query looks index-orderable; the
//! shared `finish_from_rows` tail must NOT then take its scan-order shortcut
//! (`order_satisfied_by_scan`), because the window base scan materialized the
//! rows in rowid order, not the index order that shortcut assumes. Dropping the
//! sort there yielded unsorted output (rows in rowid/insert order) whenever the
//! base table had an index — the exact bug this file guards.
//!
//! Every query carries an explicit outer `ORDER BY`, so its row order is fully
//! specified and must match sqlite3 3.50.4 byte-for-byte. Each shape is run over
//! an INDEXED table and a NON-indexed one (the non-indexed form already worked;
//! it stays green), and through BOTH engines (`set_use_vdbe(true)` VDBE window
//! path and `(false)` tree-walker) — both routed through the same tail, both
//! affected, both fixed. Plan-agnostic: only the ROWS are asserted.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

// Two identical tables — one indexed on `p`, one not — so each query can be run
// against both and compared to sqlite. The rows are deliberately inserted OUT of
// `p` order and with a `p` tie (two rows with `p = 1`) so a dropped `ORDER BY`
// (leaving rowid/insert order) is visible.
const SETUP: &str = "\
    CREATE TABLE vi(p, q);\n\
    CREATE INDEX ivi ON vi(p);\n\
    INSERT INTO vi VALUES (2, 200), (1, 100), (3, 300), (1, 101);\n\
    CREATE TABLE vn(p, q);\n\
    INSERT INTO vn VALUES (2, 200), (1, 100), (3, 300), (1, 101);\n";

// `{T}` is substituted with `vi` (indexed) or `vn` (non-indexed). Each query has
// an explicit outer ORDER BY, so the result order is specified.
const QUERIES: &[&str] = &[
    // The bug case and its DESC mirror.
    "SELECT p, count(*) OVER () FROM {T} ORDER BY p",
    "SELECT p, count(*) OVER () FROM {T} ORDER BY p DESC",
    // Other whole-partition aggregates as windows.
    "SELECT p, sum(q) OVER () FROM {T} ORDER BY p",
    "SELECT p, q, sum(q) OVER () FROM {T} ORDER BY q DESC",
    // A ranking window ordered by another column; ORDER BY on the window column.
    "SELECT p, q, row_number() OVER (ORDER BY q) AS rn FROM {T} ORDER BY q",
    "SELECT p, q, row_number() OVER (ORDER BY q) AS rn FROM {T} ORDER BY rn DESC",
    // Partitioned windows, ORDER BY on the partition key and on another column.
    "SELECT p, count(*) OVER (PARTITION BY p) AS c FROM {T} ORDER BY p, q",
    "SELECT p, q, sum(q) OVER (PARTITION BY p) AS s FROM {T} ORDER BY q",
    // Running aggregate window with ORDER BY.
    "SELECT p, q, sum(q) OVER (ORDER BY p) AS rs FROM {T} ORDER BY p, q",
    // ORDER BY on the aliased window output column, with a tiebreak.
    "SELECT p, count(*) OVER () AS c FROM {T} ORDER BY c, p",
    // With LIMIT (the sort must happen before the truncation).
    "SELECT p, count(*) OVER () FROM {T} ORDER BY p LIMIT 2",
    "SELECT p, count(*) OVER () FROM {T} ORDER BY p DESC LIMIT 2",
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

fn sqlite3_rows(query: &str) -> Vec<Vec<String>> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg("-ascii")
        .arg(format!("{SETUP}{query};"))
        .output()
        .unwrap();
    assert!(out.status.success(), "sqlite3 failed on {query}");
    let text = String::from_utf8(out.stdout).unwrap();
    text.split('\u{1e}')
        .filter(|r| !r.is_empty())
        .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
        .collect()
}

fn graphite_rows(c: &Connection, query: &str) -> Vec<Vec<String>> {
    c.query(query)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.iter().map(render).collect())
        .collect()
}

// The two engines (VDBE window path and tree-walker) must agree with each other
// for every shape, over both the indexed and non-indexed table.
#[test]
fn window_order_by_matches_across_engines() {
    let c = conn();
    for base in QUERIES {
        for table in ["vi", "vn"] {
            let q = base.replace("{T}", table);
            c.set_use_vdbe(true);
            let vdbe = graphite_rows(&c, &q);
            c.set_use_vdbe(false);
            let tree = graphite_rows(&c, &q);
            assert_eq!(vdbe, tree, "VDBE vs tree-walker diverged on {q}");
        }
    }
}

// Differential against sqlite3 3.50.4 (skip if the CLI is absent). Both engines
// must produce sqlite's exact rows AND order for every shape, indexed or not.
#[test]
fn window_order_by_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    for base in QUERIES {
        for table in ["vi", "vn"] {
            let q = base.replace("{T}", table);
            let want = sqlite3_rows(&q);
            for use_vdbe in [true, false] {
                c.set_use_vdbe(use_vdbe);
                let got = graphite_rows(&c, &q);
                assert_eq!(
                    got, want,
                    "graphite (use_vdbe={use_vdbe}) vs sqlite3 diverged on {q}"
                );
            }
        }
    }
}
