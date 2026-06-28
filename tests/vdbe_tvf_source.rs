//! A single table-valued function used as a `FROM` source now runs on the VDBE:
//! `generate_series(start[,stop[,step]])`, `json_each` / `json_tree`, and the
//! table-valued `pragma_<name>(arg)` form. `scan_one` materializes the function's
//! rows through the same `tvf_rows` the tree-walker uses, so the outer query
//! (projection / WHERE / ORDER BY / LIMIT / aggregate) sees them identically. The
//! *hidden* input columns `json_each` / `json_tree` expose (`json` / `root`) are
//! dropped — they're excluded from `*` expansion anyway, and a query naming one
//! explicitly fails to resolve on the VDBE and defers to the tree-walker.
//!
//! Deferred to the tree-walker (asserted separately), never run wrong:
//!   * a query naming a TVF's *hidden* `json` / `root` column explicitly.
//!   * any TVF appearing in a *join* — its arguments may correlate to another
//!     source's columns, which `tvf_rows` (rowless context) can't honour.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE ran the
//! TVF source itself. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES (1,'x',10),(2,'y',20),(3,'z',30);\n";

// Each query's only FROM source is a table-valued function. ORDER BY (or a
// deterministic aggregate) pins the row order for a direct comparison.
const QUERIES: &[&str] = &[
    // generate_series: default step, explicit step, descending, filtered, projected.
    "SELECT value FROM generate_series(1,5)",
    "SELECT value FROM generate_series(1,10,3)",
    "SELECT value FROM generate_series(1,5) ORDER BY value DESC",
    "SELECT value FROM generate_series(2,20,2) WHERE value > 10 ORDER BY value",
    "SELECT value * 2 AS d FROM generate_series(1,3) ORDER BY d",
    "SELECT value FROM generate_series(5,1,-2)",
    // A 0 step is treated as 1 (SQLite's quirk).
    "SELECT value FROM generate_series(1,4,0)",
    // Aggregate over the series.
    "SELECT count(*), sum(value), min(value), max(value) FROM generate_series(1,5)",
    "SELECT value % 2 AS p, count(*) FROM generate_series(1,6) GROUP BY p ORDER BY p",
    // LIMIT / OFFSET over the series.
    "SELECT value FROM generate_series(1,100) ORDER BY value LIMIT 3 OFFSET 2",
    // The table-valued pragma form.
    "SELECT name, type FROM pragma_table_info('t')",
    "SELECT name FROM pragma_table_info('t') WHERE pk = 0 ORDER BY name",
    "SELECT count(*) FROM pragma_table_info('t')",
    // json_each: named columns, `*` (hidden json/root dropped), WHERE/ORDER BY,
    // object keys, an aggregate, and a path argument.
    "SELECT key, value FROM json_each('[10,20,30]')",
    "SELECT * FROM json_each('[10,20,30]')",
    "SELECT type, value FROM json_each('[1,\"a\",2.5,true]')",
    "SELECT value FROM json_each('[10,20,30]') WHERE value > 15 ORDER BY value DESC",
    "SELECT key, value FROM json_each('{\"a\":1,\"b\":2}') ORDER BY key",
    "SELECT count(*), sum(value) FROM json_each('[1,2,3,4]')",
    "SELECT fullkey, path, value FROM json_each('{\"a\":[1,2]}','$.a')",
    // json_tree: a full recursive walk.
    "SELECT key, value, type FROM json_tree('{\"a\":1,\"b\":[2,3]}') ORDER BY id",
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
fn tvf_source_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE scanned the TVF.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn hidden_column_and_joined_tvf_fall_back() {
    let c = conn();
    // A query naming a TVF's hidden `json` / `root` column explicitly, and any TVF
    // inside a join (its args may correlate to another source), both defer to the
    // tree-walker, which still runs them.
    for q in [
        "SELECT json FROM json_each('[1,2]')",
        "SELECT je.root FROM json_each('[1,2]','$') je",
        "SELECT t.g, j.value FROM t, json_each('[1,2]') j",
        "SELECT g FROM t, generate_series(1,2)",
    ] {
        assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
        assert!(c.query(q).is_ok(), "tree-walker should run {q}");
    }
    // A TVF inside a join with a TVF-only ambiguity defers too (tree-walker errors).
    let amb = "SELECT value FROM generate_series(1,5), generate_series(1,2)";
    assert!(
        c.query_vdbe(amb).is_err(),
        "expected VDBE fallback for {amb}"
    );
}

#[test]
fn tvf_source_matches_sqlite3() {
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
