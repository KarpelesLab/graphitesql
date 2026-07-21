//! A window-function `SELECT` whose `FROM` is a **join containing a view or
//! table-valued function** now runs on the VDBE. `static_scope_columns` reads no rows
//! and reports such a join as unknown; `window_join_source_columns` then resolves each
//! source's columns exactly as the base scan's `scan_one` exposes them — a plain table
//! via `table_meta`, a view via `try_view`, a visible-masked TVF via `tvf_rows` — and
//! the base scan (`SELECT *` over the join, now column-qualified) materializes the rows.
//! The window evaluation runs in the shared `finish_from_rows` tail.
//!
//! A `NATURAL`/`USING` join (coalesced columns) runs too — see the dedicated
//! `vdbe_window_natural_join.rs` and `natural_join_view_window_runs_on_vdbe` below.
//!
//! A view column carrying a *non-BINARY* collation now runs — its collation flows
//! through to the VDBE's collation-aware paths (see
//! `nocase_view_in_join_window_runs_on_vdbe`).
//!
//! Deferred to the tree-walker (asserted separately), never run wrong:
//!   * a `rowid` reference (a joined row has no single rowid).
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE handled the
//! join window source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES (1,'x',10),(2,'y',20),(2,'z',30),(3,'w',40);\n\
    CREATE TABLE u(g INTEGER, m INTEGER);\n\
    INSERT INTO u VALUES (1,100),(2,200),(2,201);\n\
    CREATE VIEW vt AS SELECT g, n FROM t;\n\
    CREATE VIEW vu AS SELECT g AS gg, m FROM u;\n";

// Each query is a window-function SELECT whose FROM is a join with a view / TVF source.
const QUERIES: &[&str] = &[
    // A plain table joined to a view, sharing `g` (the wildcard-qualify fix applies).
    "SELECT t.a, vt.n, sum(vt.n) OVER () FROM t JOIN vt ON t.g = vt.g ORDER BY 1, 2",
    // PARTITION BY across a plain-table/view join.
    "SELECT t.a, vt.n, count(*) OVER (PARTITION BY t.g) FROM t JOIN vt ON t.g = vt.g \
     ORDER BY 1, 2",
    // Two views joined (distinct column names).
    "SELECT vt.g, vu.m, sum(vu.m) OVER (PARTITION BY vt.g) FROM vt JOIN vu ON vt.g = vu.gg \
     ORDER BY 1, 2, 3",
    // A plain table joined to a TVF.
    "SELECT t.a, s.value, row_number() OVER (ORDER BY s.value) \
     FROM t JOIN generate_series(1,2) s ON t.g = s.value ORDER BY 1, 2",
    // A view joined to a TVF.
    "SELECT vt.g, s.value, sum(vt.n) OVER () FROM vt JOIN generate_series(1,3) s ON vt.g = s.value \
     ORDER BY 1, 2",
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
fn window_join_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn window_join_matches_sqlite3() {
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
fn natural_join_view_window_runs_on_vdbe() {
    let c = conn();
    // A NATURAL join coalesces the shared columns; `window_join_source_columns` now
    // mirrors the base scan's coalescing, so the join window source runs on the VDBE
    // and matches the tree-walker (covered broadly in vdbe_window_natural_join.rs).
    let q = "SELECT g, sum(n) OVER () FROM t NATURAL JOIN vt ORDER BY 1, 2";
    let got = c.query_vdbe(q).unwrap().rows;
    let want = c.query(q).unwrap().rows;
    assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
}

#[test]
fn nocase_view_in_join_window_runs_on_vdbe() {
    let mut c = conn();
    c.execute("CREATE TABLE w(k TEXT COLLATE NOCASE, v INTEGER)")
        .unwrap();
    c.execute("INSERT INTO w VALUES ('A',1),('b',2)").unwrap();
    c.execute("CREATE VIEW vw AS SELECT k, v FROM w").unwrap();
    // The view column `k` carries NOCASE; its collation flows through to the VDBE, so
    // the windowed join over the view runs there — matching the tree-walker and SQLite.
    let q = "SELECT t.a, vw.k, count(*) OVER () FROM t JOIN vw ON t.g = vw.v";
    let got = c
        .query_vdbe(q)
        .unwrap_or_else(|e| panic!("expected VDBE to run {q}: {e}"));
    assert_eq!(
        got.rows,
        c.query(q).unwrap().rows,
        "VDBE vs tree-walker on {q}"
    );
}
