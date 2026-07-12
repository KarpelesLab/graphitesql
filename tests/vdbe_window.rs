//! Track B5c-4: window functions over a single base table — or a plain N-table
//! join — run on the VDBE. The source is scanned (with `WHERE` applied; for a
//! single table its rowid is appended) as bytecode, and the window evaluation /
//! projection / DISTINCT / ORDER BY / LIMIT reuse the same tree-walker tail
//! (`finish_from_rows`). `query_vdbe` errors on any fallback, so a passing query
//! proves the VDBE compiled the scan; results match the tree-walker and sqlite
//! 3.50.4. A windowed join that *references* a rowid still defers (a joined row
//! has no single rowid).
//!
//! Each query's outer `ORDER BY` is deliberately *not* a sole rowid term (it
//! references the window column or carries a tiebreak), so it is not "satisfied
//! by the scan" — otherwise `run_select_vdbe` defers before the window path. The
//! join's two tables carry disjoint column names (the VDBE join resolver rejects
//! a name shared across tables), and each windowed join orders by the unique
//! `(t.id, u.uid)` so the differential row order is total.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => String::from(s.as_str()),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

const SETUP: &str = "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT, s TEXT);\n\
     INSERT INTO t(a,b,s) VALUES (1,3,'c'),(1,1,'a'),(1,2,'b'),(2,5,'z'),(2,4,'y'),(1,NULL,'d');\n\
     CREATE TABLE u(uid INTEGER PRIMARY KEY, k INT, v INT);\n\
     INSERT INTO u(k,v) VALUES (1,10),(2,20),(2,30);\n";

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

// Window functions over a plain two-table join (`t.a = u.k`). None references a
// rowid (a joined row has none); each orders by the unique `(t.id, u.uid)`.
const JOINED: &[&str] = &[
    "SELECT t.id, u.v, row_number() OVER (ORDER BY t.id, u.uid) AS rn \
     FROM t JOIN u ON t.a = u.k ORDER BY t.id, u.uid",
    "SELECT t.id, count(*) OVER () AS c, sum(u.v) OVER (PARTITION BY t.a) AS sv \
     FROM t JOIN u ON t.a = u.k ORDER BY t.id, u.uid",
    "SELECT t.id, rank() OVER (ORDER BY t.a) AS rk \
     FROM t JOIN u ON t.a = u.k ORDER BY t.id, u.uid",
    "SELECT t.id, u.v, sum(u.v) OVER (PARTITION BY t.a ORDER BY t.b, t.id, u.uid) AS rps \
     FROM t JOIN u ON t.a = u.k WHERE t.b IS NOT NULL ORDER BY t.id, u.uid",
];

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT, s TEXT)")
        .unwrap();
    c.execute("INSERT INTO t(a,b,s) VALUES (1,3,'c'),(1,1,'a'),(1,2,'b'),(2,5,'z'),(2,4,'y'),(1,NULL,'d')")
        .unwrap();
    c.execute("CREATE TABLE u(uid INTEGER PRIMARY KEY, k INT, v INT)")
        .unwrap();
    c.execute("INSERT INTO u(k,v) VALUES (1,10),(2,20),(2,30)")
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
    for q in QUERIES.iter().chain(GROUPED).chain(JOINED) {
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
    for q in QUERIES.iter().chain(GROUPED).chain(JOINED) {
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

/// The rowid gate on the join path: two otherwise-identical windowed joins differ
/// only in a `rowid` reference. A joined row has no single rowid, so the VDBE runs
/// the first and defers the second to the tree-walker.
#[test]
fn window_over_join_with_rowid_defers() {
    let c = conn();
    let ok = "SELECT t.id, row_number() OVER (ORDER BY t.id, u.uid) AS rn \
              FROM t JOIN u ON t.a = u.k ORDER BY t.id, u.uid";
    let with_rowid = "SELECT t.id, row_number() OVER (ORDER BY t.rowid) AS rn \
                      FROM t JOIN u ON t.a = u.k ORDER BY t.id, u.uid";
    assert!(
        c.query_vdbe(ok).is_ok(),
        "a non-rowid windowed join should run on the VDBE"
    );
    assert!(
        c.query_vdbe(with_rowid).is_err(),
        "a rowid-referencing windowed join should defer off the VDBE"
    );
}
