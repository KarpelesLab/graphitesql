//! Track B: `DISTINCT` aggregates on the bare single-table aggregate path run on
//! the VDBE (`count(DISTINCT x)`, `sum`/`avg`/`total`/`min`/`max`/`group_concat`
//! with `DISTINCT`). The collected argument values are deduped at fold time under
//! BINARY equality (a non-BINARY column collation still defers to the
//! tree-walker). `query_vdbe` errors on any fallback, so these passing proves the
//! VDBE compiled them; results match the tree-walker and sqlite 3.50.4.

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
    "SELECT count(DISTINCT a) FROM t",
    "SELECT sum(DISTINCT a) FROM t",
    "SELECT avg(DISTINCT a) FROM t",
    "SELECT total(DISTINCT a) FROM t",
    "SELECT min(DISTINCT a), max(DISTINCT a) FROM t",
    "SELECT group_concat(DISTINCT s) FROM t",
    // DISTINCT and plain aggregates side by side in one row.
    "SELECT count(DISTINCT a), count(DISTINCT b), count(*) FROM t",
    // DISTINCT aggregate with a WHERE filter applied before the fold.
    "SELECT count(DISTINCT a) FROM t WHERE b > 2",
    // DISTINCT over a computed argument expression.
    "SELECT count(DISTINCT a % 2) FROM t",
    // Empty input: count → 0, the rest → NULL.
    "SELECT count(DISTINCT a), sum(DISTINCT a) FROM t WHERE a > 100",
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
fn distinct_aggregates_run_on_vdbe_and_match_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // query_vdbe errors on any fallback, so success proves the VDBE handled it.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn distinct_aggregates_match_sqlite3() {
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

/// A `DISTINCT` aggregate over a non-BINARY column collation must defer to the
/// tree-walker (the VDBE aggregate path dedups under BINARY only). The
/// tree-walker still produces the correct answer.
#[test]
fn distinct_aggregate_over_nocase_defers_but_is_correct() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(x TEXT COLLATE NOCASE)").unwrap();
    c.execute("INSERT INTO u VALUES ('a'),('A'),('b')").unwrap();
    // The VDBE bails (non-BINARY collation), so query_vdbe errors …
    assert!(c.query_vdbe("SELECT count(DISTINCT x) FROM u").is_err());
    // … but the default path (tree-walker fallback) matches sqlite: 'a'=='A'.
    let r = c.query("SELECT count(DISTINCT x) FROM u").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(2)]]);
}
