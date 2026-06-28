//! A window-function `SELECT` whose `FROM` source is a **table-valued function** now
//! runs on the VDBE (Track B5c-4 extension). `run_window_vdbe` resolves the TVF's
//! *visible* column model through `tvf_rows` (masking the hidden `json`/`root` input
//! columns), and the base scan materializes the rows through `scan_one`'s TVF branch,
//! so columns and rows stay in lockstep. The window evaluation runs in the shared
//! `finish_from_rows` tail, exactly as for a plain-table or view window source.
//!
//! Covered sources: `generate_series(start[,stop[,step]])`, `json_each` / `json_tree`,
//! and the table-valued `pragma_<name>(arg)` form.
//!
//! A TVF inside a *join* window source also runs now (its columns resolve through
//! `window_join_source_columns`), asserted by `tvf_in_cross_join_window_runs_on_vdbe`.
//!
//! Deferred to the tree-walker (asserted separately), never run wrong:
//!   * a `rowid` reference over the TVF — a TVF row has no rowid (like a derived / CTE
//!     / view window source), so any `rowid`/`_rowid_`/`oid` reference defers.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE handled the
//! TVF window source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES (1,'x',10),(2,'y',20),(2,'z',30),(3,'w',40);\n";

// Each query is a window-function SELECT whose FROM source is a table-valued function.
// A final ORDER BY pins the row order for a direct comparison.
const QUERIES: &[&str] = &[
    // A running sum over generate_series.
    "SELECT value, sum(value) OVER (ORDER BY value) FROM generate_series(1,5) ORDER BY 1",
    // row_number over a whole-partition window.
    "SELECT value, row_number() OVER () FROM generate_series(2,8,2) ORDER BY 1, 2",
    // count(*) OVER () over json_each elements (visible key/value columns).
    "SELECT key, value, count(*) OVER () FROM json_each('[10,20,30]') ORDER BY 1",
    // A PARTITION BY over json_each values.
    "SELECT value, sum(value) OVER (PARTITION BY value % 2) FROM json_each('[1,2,3,4]') \
     ORDER BY 1, 2",
    // rank() over json_tree (atom + container rows).
    "SELECT type, rank() OVER (ORDER BY type) FROM json_tree('[1,2]') ORDER BY 1, 2",
    // A window over the table-valued pragma_<name>(arg) form.
    "SELECT name, count(*) OVER () FROM pragma_table_info('t') ORDER BY 1",
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
fn window_tvf_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE scanned the TVF.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn window_tvf_matches_sqlite3() {
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

#[test]
fn rowid_over_tvf_window_defers() {
    let c = conn();
    // A TVF row has no rowid; a `rowid` reference in the window query defers.
    let q = "SELECT value, sum(value) OVER () FROM generate_series(1,3) WHERE rowid > 0";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
}

#[test]
fn tvf_in_cross_join_window_runs_on_vdbe() {
    let c = conn();
    // A TVF inside a (comma / cross) join window source now resolves its columns via
    // `window_join_source_columns` and runs on the VDBE — matching the tree-walker.
    let q = "SELECT t.g, s.value, sum(s.value) OVER () FROM t JOIN generate_series(1,2) s \
             ORDER BY 1, 2";
    let got = c.query_vdbe(q).unwrap().rows;
    let want = c.query(q).unwrap().rows;
    assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
}
