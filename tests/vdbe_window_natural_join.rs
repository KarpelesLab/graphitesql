//! A window-function `SELECT` whose `FROM` is a **`NATURAL` or `USING` join** now runs
//! on the VDBE. Such a join *coalesces* its shared columns into a single output column,
//! so `window_join_source_columns` mirrors the base scan's coalescing: it resolves each
//! source's columns through `window_join_one_source` (a plain table via `table_meta`, a
//! view via `try_view`, a derived subquery via `window_source_columns`, a TVF via
//! `tvf_rows`), then — for a `NATURAL` join (every same-named pair) or a `USING (…)` join
//! (the named columns) — drops the right-hand duplicate of each coalesced pair, keeping
//! the **left** column's name/affinity/collation. The column-qualified `SELECT *` base
//! scan materializes the same coalesced rows, and the window evaluation runs in the
//! shared `finish_from_rows` tail.
//!
//! `query_vdbe` errors on any fallback, so a passing query proves the VDBE handled the
//! coalesced join window source. Checked against the tree-walker and sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, n INTEGER);\n\
    INSERT INTO t VALUES (1,10),(2,20),(2,30),(3,40);\n\
    CREATE TABLE u(g INTEGER, m INTEGER);\n\
    INSERT INTO u VALUES (1,100),(2,200),(2,201);\n\
    CREATE VIEW vt AS SELECT g, n FROM t;\n\
    CREATE TABLE w(g INTEGER, k TEXT COLLATE NOCASE);\n\
    INSERT INTO w VALUES (1,'A'),(2,'b'),(3,'C');\n";

// Each query is a window-function SELECT whose FROM is a NATURAL or USING join. A final
// ORDER BY pins the row order for a direct comparison.
const QUERIES: &[&str] = &[
    // NATURAL join of a plain table and a view (coalesces g and n).
    "SELECT g, sum(n) OVER () FROM t NATURAL JOIN vt ORDER BY 1, 2",
    // USING-join with a PARTITION BY over the coalesced key.
    "SELECT g, count(*) OVER (PARTITION BY g) FROM t JOIN u USING (g) ORDER BY 1",
    // USING-join projecting a right-hand non-coalesced column too.
    "SELECT g, m, sum(m) OVER () FROM t JOIN u USING (g) ORDER BY 1, 2",
    // NATURAL join coalescing only g (w carries an extra non-shared column).
    "SELECT g, sum(g) OVER () FROM w NATURAL JOIN u ORDER BY 1, 2",
    // USING-join projecting a coalesced key plus a non-shared text column.
    "SELECT g, k, count(*) OVER () FROM w JOIN u USING (g) ORDER BY 1, 2",
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
fn window_natural_join_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the VDBE ran the coalesced join.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn window_natural_join_matches_sqlite3() {
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
fn nocase_coalesced_partition_matches_tree_walker() {
    // When the *coalesced* column itself carries a non-BINARY collation, the VDBE keeps
    // the left column's collation exactly as the base scan does — so the window result
    // stays byte-identical to the tree-walker even on this collation edge.
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "CREATE TABLE p(k TEXT COLLATE NOCASE, n INTEGER)",
        "INSERT INTO p VALUES ('A',1),('a',2),('B',3)",
        "CREATE TABLE q(k TEXT COLLATE NOCASE, m INTEGER)",
        "INSERT INTO q VALUES ('a',10),('b',20)",
    ] {
        c.execute(s).unwrap();
    }
    let q = "SELECT k, count(*) OVER (PARTITION BY k) FROM p JOIN q USING (k) ORDER BY k, 2";
    let got = c.query_vdbe(q).unwrap().rows;
    let want = c.query(q).unwrap().rows;
    assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
}
