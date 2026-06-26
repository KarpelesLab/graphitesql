//! Track B5c-4: window functions over a single base table run on the VDBE. The
//! base table is scanned (with `WHERE` applied, rowid appended) as bytecode, and
//! the window evaluation / projection / DISTINCT / ORDER BY / LIMIT reuse the same
//! tree-walker tail (`finish_from_rows`). `query_vdbe` errors on any fallback, so
//! a passing query proves the VDBE compiled the scan; results match the
//! tree-walker and sqlite 3.50.4. A window over a join still defers.
//!
//! Each query's outer `ORDER BY` is deliberately *not* a sole rowid term (it
//! references the window column or carries a tiebreak), so it is not "satisfied
//! by the scan" — otherwise `run_select_vdbe` defers before the window path.

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

const SETUP: &str = "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT, s TEXT);\n\
     INSERT INTO t(a,b,s) VALUES (1,3,'c'),(1,1,'a'),(1,2,'b'),(2,5,'z'),(2,4,'y'),(1,NULL,'d');\n";

const QUERIES: &[&str] = &[
    // Ranking functions.
    "SELECT id, row_number() OVER (ORDER BY b, id) AS rn FROM t ORDER BY rn",
    "SELECT id, rank() OVER (ORDER BY a) AS rk FROM t ORDER BY rk, id",
    "SELECT id, dense_rank() OVER (ORDER BY a) AS dr FROM t ORDER BY dr, id",
    "SELECT id, ntile(2) OVER (ORDER BY id) AS nt FROM t ORDER BY id, a",
    // Aggregates as windows: whole-partition and running.
    "SELECT id, count(*) OVER () AS c FROM t ORDER BY id, a",
    "SELECT id, sum(b) OVER (ORDER BY b, id) AS rs FROM t ORDER BY id, a",
    "SELECT id, avg(b) OVER (PARTITION BY a) AS av FROM t ORDER BY id, a",
    "SELECT id, sum(b) OVER (PARTITION BY a ORDER BY b, id) AS rps FROM t ORDER BY id, a",
    // Offset / value functions.
    "SELECT id, lag(b) OVER (ORDER BY id) AS lg FROM t ORDER BY id, a",
    "SELECT id, lead(b, 1, -1) OVER (ORDER BY id) AS ld FROM t ORDER BY id, a",
    "SELECT id, first_value(b) OVER (PARTITION BY a ORDER BY id) AS fv FROM t ORDER BY id, a",
    // Explicit frame.
    "SELECT id, sum(b) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS w FROM t ORDER BY id, a",
    // Named window definition.
    "SELECT id, sum(b) OVER w AS sw FROM t WINDOW w AS (ORDER BY id) ORDER BY id, a",
    // Window combined with a WHERE filter (the scan applies it).
    "SELECT id, row_number() OVER (ORDER BY b, id) AS rn FROM t WHERE a = 1 ORDER BY rn",
];

// Window functions evaluated *over* GROUP BY rows (the windowed-aggregate path).
const GROUPED: &[&str] = &[
    "SELECT a, sum(b) AS sb, sum(sum(b)) OVER () AS tot FROM t GROUP BY a ORDER BY a",
    "SELECT a, count(*) AS c, rank() OVER (ORDER BY count(*) DESC) AS rk FROM t GROUP BY a ORDER BY a",
];

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT, s TEXT)")
        .unwrap();
    c.execute("INSERT INTO t(a,b,s) VALUES (1,3,'c'),(1,1,'a'),(1,2,'b'),(2,5,'z'),(2,4,'y'),(1,NULL,'d')")
        .unwrap();
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

#[test]
fn window_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES.iter().chain(GROUPED) {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn window_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    for q in QUERIES.iter().chain(GROUPED) {
        let vdbe: Vec<Vec<String>> = c
            .query_vdbe(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| r.iter().map(render).collect())
            .collect();
        assert_eq!(vdbe, sqlite3_rows(q), "VDBE vs sqlite3 diverged on {q}");
    }
}

/// A window function over a join is outside the single-table scope, so the VDBE
/// defers; the tree-walker fallback still answers it correctly.
#[test]
fn window_over_join_defers() {
    let c = conn();
    let q = "SELECT x.id, row_number() OVER (ORDER BY x.id) AS rn \
             FROM t x JOIN t y ON x.a = y.a ORDER BY rn, x.id";
    assert!(
        c.query_vdbe(q).is_err(),
        "expected the VDBE to defer a windowed join"
    );
    // The tree-walker handles it.
    assert!(!c.query(q).unwrap().rows.is_empty());
}
