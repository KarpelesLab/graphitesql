//! A window-function `SELECT` whose `FROM` source is a **view** now runs on the VDBE
//! (Track B5c-4 extension). `run_window_vdbe` resolves the view's per-column
//! `(affinity, collation)` through `try_view` — the same model the non-window view
//! scan exposes — and the base scan materializes the view body through `scan_one`, so
//! columns and rows stay in lockstep. The window evaluation itself runs in the shared
//! `finish_from_rows` tail, exactly as for a plain-table window source.
//!
//! A view column carrying a *non-BINARY* collation now runs — its collation flows
//! through to the VDBE's collation-aware paths (see
//! `nocase_view_column_window_runs_on_vdbe`).
//!
//! Deferred to the tree-walker (asserted separately), never run wrong:
//!   * a `rowid` reference over the view — a view has no rowid, so (like a derived /
//!     CTE window source) any `rowid`/`_rowid_`/`oid` reference defers. A view body
//!     that *projects* `rowid` under an alias exposes an ordinary column that runs.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE handled the
//! view window source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES (1,'x',10),(2,'y',20),(2,'z',30),(3,'w',40);\n\
    CREATE TABLE u(g INTEGER, m INTEGER);\n\
    INSERT INTO u VALUES (1,100),(2,200),(2,201);\n\
    CREATE VIEW vt AS SELECT g, n FROM t;\n\
    CREATE VIEW vj AS SELECT t.g AS g, u.m AS m FROM t JOIN u ON t.g=u.g;\n\
    CREATE VIEW vc AS SELECT g FROM t UNION SELECT m FROM u;\n\
    CREATE VIEW vrowid AS SELECT rowid AS r, g FROM t;\n";

// Each query is a window-function SELECT whose FROM source is a view. A final ORDER BY
// pins the row order for a direct comparison.
const QUERIES: &[&str] = &[
    // PARTITION BY over a view column.
    "SELECT g, sum(n) OVER (PARTITION BY g) FROM vt ORDER BY 1, 2",
    // row_number with an affinity-sensitive ORDER BY in the window spec.
    "SELECT g, row_number() OVER (ORDER BY n DESC) FROM vt ORDER BY 1, 2",
    // A running sum (default frame) over a view column.
    "SELECT n, sum(n) OVER (ORDER BY n) FROM vt ORDER BY 1",
    // A whole-partition count over the view.
    "SELECT g, count(*) OVER () FROM vt ORDER BY 1, 2",
    // A join-bodied view as the window source.
    "SELECT g, m, sum(m) OVER (PARTITION BY g) FROM vj ORDER BY 1, 2, 3",
    // A compound-bodied (UNION) view as the window source.
    "SELECT g, rank() OVER (ORDER BY g) FROM vc ORDER BY 1, 2",
    // A view body that projects an aliased rowid is an ordinary column the window uses.
    "SELECT r, g, sum(g) OVER (ORDER BY r) FROM vrowid ORDER BY 1",
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
fn window_view_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE scanned the view.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn window_view_matches_sqlite3() {
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
fn nocase_view_column_window_runs_on_vdbe() {
    let mut c = conn();
    c.execute("CREATE TABLE w(k TEXT COLLATE NOCASE, val INTEGER)")
        .unwrap();
    c.execute("INSERT INTO w VALUES ('A',1),('b',2)").unwrap();
    c.execute("CREATE VIEW vw AS SELECT k, val FROM w").unwrap();
    // The view column `k` carries NOCASE; its collation flows through to the VDBE, so
    // the windowed query over the view (with an ORDER BY on the collated column) runs
    // there — matching the tree-walker and SQLite.
    let q = "SELECT k, count(*) OVER () FROM vw ORDER BY 1";
    let got = c
        .query_vdbe(q)
        .unwrap_or_else(|e| panic!("expected VDBE to run {q}: {e}"));
    assert_eq!(
        got.rows,
        c.query(q).unwrap().rows,
        "VDBE vs tree-walker on {q}"
    );
}

#[test]
fn rowid_over_view_window_defers() {
    let c = conn();
    // A view has no rowid; a `rowid` reference in the window query defers (the VDBE
    // base scan supplies no per-row rowid for a view source).
    let q = "SELECT g, sum(n) OVER () FROM vt WHERE rowid > 0";
    assert!(c.query_vdbe(q).is_err(), "expected VDBE fallback for {q}");
}
