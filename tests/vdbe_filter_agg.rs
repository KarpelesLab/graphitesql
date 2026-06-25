//! Track B: aggregate `FILTER (WHERE …)` runs on the VDBE — on the bare
//! single-table path, over `GROUP BY`, and over a two-table join, and composing
//! with `DISTINCT`. The filter predicate is evaluated per row and gates whether
//! that row folds into the aggregate (for `count(*)` it gates the count bump);
//! each aggregate's filter is independent. `query_vdbe` errors on any fallback,
//! so these passing proves the VDBE compiled them; results match the tree-walker
//! and sqlite 3.50.4.

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
     INSERT INTO t(a,b,s) VALUES (3,10,'x'),(1,2,'y'),(2,5,'x'),(1,3,'z'),(2,7,'x'),(1,1,'y');\n";

const QUERIES: &[&str] = &[
    // count(*) / count(col) with a FILTER predicate.
    "SELECT count(*) FILTER (WHERE b > 2) FROM t",
    "SELECT count(a) FILTER (WHERE b > 2) FROM t",
    // sum/avg/total/min/max/group_concat under FILTER.
    "SELECT sum(b) FILTER (WHERE a = 1) FROM t",
    "SELECT avg(b) FILTER (WHERE a = 1) FROM t",
    "SELECT total(b) FILTER (WHERE a = 1) FROM t",
    "SELECT min(b) FILTER (WHERE a <> 1), max(b) FILTER (WHERE a <> 1) FROM t",
    "SELECT group_concat(s) FILTER (WHERE b > 2) FROM t",
    // Several aggregates, each with its own (different) FILTER, in one row.
    "SELECT count(*) FILTER (WHERE b > 2), count(*) FILTER (WHERE a = 1), count(*) FROM t",
    // FILTER composed with DISTINCT: filter first, then dedup.
    "SELECT count(DISTINCT a) FILTER (WHERE b > 2) FROM t",
    "SELECT sum(DISTINCT a) FILTER (WHERE b < 100) FROM t",
    // A FILTER and a plain WHERE together (WHERE first, then per-agg FILTER).
    "SELECT count(*) FILTER (WHERE b > 2) FROM t WHERE a <> 3",
    // Empty filter result: count → 0, sum/group_concat → NULL.
    "SELECT count(*) FILTER (WHERE b > 100), sum(b) FILTER (WHERE b > 100) FROM t",
    // FILTER over a computed predicate.
    "SELECT count(*) FILTER (WHERE a % 2 = 1) FROM t",
];

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT, s TEXT)")
        .unwrap();
    c.execute(
        "INSERT INTO t(a,b,s) VALUES (3,10,'x'),(1,2,'y'),(2,5,'x'),(1,3,'z'),(2,7,'x'),(1,1,'y')",
    )
    .unwrap();
    c
}

#[test]
fn filter_aggregates_run_on_vdbe_and_match_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // query_vdbe errors on any fallback, so success proves the VDBE handled it.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn filter_aggregates_match_sqlite3() {
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
        let expected: Vec<Vec<String>> = text
            .split('\u{1e}')
            .filter(|r| !r.is_empty())
            .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
            .collect();
        assert_eq!(vdbe, expected, "VDBE vs sqlite3 diverged on {q}");
    }
}

// ── FILTER aggregates over GROUP BY ─────────────────────────────────────────
const GSETUP: &str = "CREATE TABLE g(id INTEGER PRIMARY KEY, a INT, b INT, s TEXT);\n\
     INSERT INTO g(a,b,s) VALUES (1,10,'x'),(1,10,'y'),(1,20,'x'),(2,5,'z'),(2,5,'z'),(3,7,'q');\n";

const GROUPED: &[&str] = &[
    // Plain (GroupEmit) path: a per-group FILTER aggregate.
    "SELECT a, count(*) FILTER (WHERE b > 5) FROM g GROUP BY a ORDER BY a",
    // FILTER mixed with plain aggregates and group_concat in one grouped row.
    "SELECT a, count(*) FILTER (WHERE b > 5), count(*), group_concat(s) FILTER (WHERE s <> 'x') \
     FROM g GROUP BY a ORDER BY a",
    // FILTER composed with DISTINCT, per group.
    "SELECT a, count(DISTINCT b) FILTER (WHERE b <> 10) FROM g GROUP BY a ORDER BY a",
    // sum/avg under FILTER per group.
    "SELECT a, sum(b) FILTER (WHERE b > 5), avg(b) FILTER (WHERE b > 5) FROM g GROUP BY a ORDER BY a",
    // General path: a FILTER aggregate inside HAVING, plus ORDER BY/LIMIT.
    "SELECT a, sum(b) FILTER (WHERE b > 0) AS sb FROM g GROUP BY a \
     HAVING count(*) FILTER (WHERE b > 5) >= 1 ORDER BY sb DESC, a LIMIT 2",
];

fn gconn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE g(id INTEGER PRIMARY KEY, a INT, b INT, s TEXT)")
        .unwrap();
    c.execute(
        "INSERT INTO g(a,b,s) VALUES (1,10,'x'),(1,10,'y'),(1,20,'x'),(2,5,'z'),(2,5,'z'),(3,7,'q')",
    )
    .unwrap();
    c
}

#[test]
fn filter_aggregates_over_group_by_run_on_vdbe() {
    let c = gconn();
    for q in GROUPED {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn filter_aggregates_over_group_by_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = gconn();
    for q in GROUPED {
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
            .arg(format!("{GSETUP}{q};"))
            .output()
            .unwrap();
        assert!(out.status.success(), "sqlite3 failed on {q}");
        let text = String::from_utf8(out.stdout).unwrap();
        let expected: Vec<Vec<String>> = text
            .split('\u{1e}')
            .filter(|r| !r.is_empty())
            .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
            .collect();
        assert_eq!(vdbe, expected, "VDBE vs sqlite3 diverged on {q}");
    }
}

/// FILTER aggregates over a two-table join run on the VDBE join aggregate path,
/// composing with DISTINCT. Spot-checked against the hand-computed values.
#[test]
fn filter_aggregate_over_join_runs_on_vdbe() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(id INTEGER PRIMARY KEY, k INT, v INT)")
        .unwrap();
    c.execute("CREATE TABLE b(id INTEGER PRIMARY KEY, k INT, w INT)")
        .unwrap();
    c.execute("INSERT INTO a(k,v) VALUES (1,10),(1,20),(2,30)")
        .unwrap();
    c.execute("INSERT INTO b(k,w) VALUES (1,100),(1,200),(2,300),(2,300)")
        .unwrap();
    let q = "SELECT count(*) FILTER (WHERE b.w > 150), \
             sum(b.w) FILTER (WHERE a.v = 10), \
             count(DISTINCT b.w) FILTER (WHERE b.w >= 200) \
             FROM a JOIN b ON a.k = b.k";
    let got = c.query_vdbe(q).unwrap().rows;
    let want = c.query(q).unwrap().rows;
    assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    // 6 joined rows; w>150 → 4 rows; rows with a.v=10 carry w∈{100,200} → 300;
    // distinct w≥200 over {200,200,300,300} → {200,300} = 2.
    assert_eq!(
        got,
        vec![vec![
            Value::Integer(4),
            Value::Integer(300),
            Value::Integer(2)
        ]]
    );
}
