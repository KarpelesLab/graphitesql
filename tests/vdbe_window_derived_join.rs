//! A window-function `SELECT` whose `FROM` is a **join containing a derived subquery**
//! now runs on the VDBE. `static_scope_columns` reads no rows and reports such a join
//! as unknown; `window_join_source_columns` then resolves each source's columns exactly
//! as the base scan exposes them — a plain table via `table_meta`, a view via
//! `try_view`, a visible-masked TVF via `tvf_rows`, and a **derived subquery via
//! `window_source_columns`** (the same `(affinity, collation)` model the single-source
//! derived window path uses) — and the column-qualified `SELECT *` base scan
//! materializes the joined rows. The window evaluation runs in the shared
//! `finish_from_rows` tail.
//!
//! A derived column carrying a *non-BINARY* collation now runs — its collation flows
//! through to the VDBE's collation-aware paths (see
//! `nocase_derived_in_join_window_runs_on_vdbe`).
//!
//! Deferred to the tree-walker (asserted separately), never run wrong:
//!   * a `NATURAL`/`USING` join (coalesced columns) — still resolved structurally.
//!   * a `rowid` reference (a joined row has no single rowid).
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE handled the
//! derived join window source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT, n INTEGER);\n\
    INSERT INTO t VALUES (1,'x',10),(2,'y',20),(2,'z',30),(3,'w',40);\n\
    CREATE TABLE u(g INTEGER, m INTEGER);\n\
    INSERT INTO u VALUES (1,100),(2,200),(2,201);\n\
    CREATE VIEW vt AS SELECT g, n FROM t;\n";

// Each query is a window-function SELECT whose FROM is a join with a derived source.
const QUERIES: &[&str] = &[
    // A derived subquery joined to a plain table.
    "SELECT x.g, sum(x.n) OVER () FROM (SELECT g, n FROM t) x JOIN u ON x.g = u.g \
     ORDER BY 1, 2",
    // PARTITION BY across a derived / plain-table join.
    "SELECT x.g, u.m, count(*) OVER (PARTITION BY x.g) FROM (SELECT g, n FROM t) x \
     JOIN u ON x.g = u.g ORDER BY 1, 2",
    // A derived subquery joined to a view.
    "SELECT x.n, vt.n, sum(vt.n) OVER () FROM (SELECT g, n FROM t) x JOIN vt ON x.g = vt.g \
     ORDER BY 1, 2",
    // A derived subquery joined to a table-valued function.
    "SELECT x.g, s.value, row_number() OVER (ORDER BY s.value) \
     FROM (SELECT g FROM t) x JOIN generate_series(1, 2) s ON x.g = s.value ORDER BY 1, 2",
    // Two derived sources joined.
    "SELECT a.g, b.m, sum(b.m) OVER () FROM (SELECT g FROM t) a JOIN (SELECT g, m FROM u) b \
     ON a.g = b.g ORDER BY 1, 2",
    // A derived source whose own body is a join.
    "SELECT x.g, sum(x.n) OVER () FROM (SELECT t.g, t.n FROM t JOIN u ON t.g = u.g) x \
     JOIN u ON x.g = u.g ORDER BY 1, 2",
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
fn window_derived_join_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn window_derived_join_matches_sqlite3() {
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
fn nocase_derived_in_join_window_runs_on_vdbe() {
    let mut c = conn();
    c.execute("CREATE TABLE w(k TEXT COLLATE NOCASE, v INTEGER)")
        .unwrap();
    c.execute("INSERT INTO w VALUES ('A',1),('b',2)").unwrap();
    // The derived column `k` carries NOCASE; its origin resolves through the derived
    // body, so the collation flows through to the VDBE and the windowed join query
    // runs there — matching the tree-walker and SQLite.
    let q = "SELECT x.k, count(*) OVER () FROM (SELECT k, v FROM w) x JOIN u ON x.v = u.g";
    let got = c
        .query_vdbe(q)
        .unwrap_or_else(|e| panic!("expected VDBE to run {q}: {e}"));
    assert_eq!(
        got.rows,
        c.query(q).unwrap().rows,
        "VDBE vs tree-walker on {q}"
    );
}
