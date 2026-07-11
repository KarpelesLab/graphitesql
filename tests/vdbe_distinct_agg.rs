//! Track B: `DISTINCT` aggregates run on the VDBE (`count(DISTINCT x)`,
//! `sum`/`avg`/`total`/`min`/`max`/`group_concat` with `DISTINCT`) — on the bare
//! single-table path, over `GROUP BY`, and over a two-table join. The collected
//! argument values are deduped at fold time under BINARY equality (a non-BINARY
//! column collation still defers to the tree-walker). `query_vdbe` errors on any
//! fallback, so these passing proves the VDBE compiled them; results match the
//! tree-walker and sqlite 3.50.4.

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

// ── DISTINCT aggregates over GROUP BY ──────────────────────────────────────
// A second table with WITHIN-group duplicate values so the per-group dedup is
// actually exercised: group a=1 has b={10,10,20} and s={x,y,x}; a=2 has b={5,5}.
const GSETUP: &str = "CREATE TABLE g(id INTEGER PRIMARY KEY, a INT, b INT, s TEXT);\n\
     INSERT INTO g(a,b,s) VALUES (1,10,'x'),(1,10,'y'),(1,20,'x'),(2,5,'z'),(2,5,'z'),(3,7,'q');\n";

const GROUPED: &[&str] = &[
    // Plain (GroupEmit) path: DISTINCT aggregate, no HAVING/ORDER BY/LIMIT.
    "SELECT a, count(DISTINCT b) FROM g GROUP BY a ORDER BY a",
    // DISTINCT mixed with plain aggregates in one grouped row.
    "SELECT a, count(DISTINCT b), count(*), group_concat(DISTINCT s) FROM g GROUP BY a ORDER BY a",
    // sum/avg DISTINCT per group.
    "SELECT a, sum(DISTINCT b), avg(DISTINCT b) FROM g GROUP BY a ORDER BY a",
    // General path: a DISTINCT aggregate inside HAVING.
    "SELECT a, sum(DISTINCT b) FROM g GROUP BY a HAVING count(DISTINCT b) >= 2 ORDER BY a",
    // ORDER BY a DISTINCT aggregate, with LIMIT.
    "SELECT a, count(DISTINCT b) AS n FROM g GROUP BY a ORDER BY n DESC, a LIMIT 2",
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
fn distinct_aggregates_over_group_by_run_on_vdbe() {
    let c = gconn();
    for q in GROUPED {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn distinct_aggregates_over_group_by_match_sqlite3() {
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

/// A bare `DISTINCT` aggregate over a two-table join runs on the VDBE join
/// aggregate path. The join replicates `b.w=300` across the matched rows, so the
/// dedup is meaningful; results match sqlite 3.50.4.
#[test]
fn distinct_aggregate_over_join_runs_on_vdbe() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(id INTEGER PRIMARY KEY, k INT, v INT)")
        .unwrap();
    c.execute("CREATE TABLE b(id INTEGER PRIMARY KEY, k INT, w INT)")
        .unwrap();
    c.execute("INSERT INTO a(k,v) VALUES (1,10),(1,20),(2,30)")
        .unwrap();
    c.execute("INSERT INTO b(k,w) VALUES (1,100),(1,200),(2,300),(2,300)")
        .unwrap();
    for q in [
        "SELECT count(DISTINCT a.v), count(DISTINCT b.w), count(*) FROM a JOIN b ON a.k=b.k",
        "SELECT sum(DISTINCT b.w) FROM a JOIN b ON a.k=b.k",
    ] {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
    // Spot-check the actual values: 3 distinct v, 3 distinct w (300 collapses), 6
    // joined rows; sum of distinct w = 100+200+300 = 600.
    let r = c
        .query_vdbe(
            "SELECT count(DISTINCT a.v), count(DISTINCT b.w), count(*) FROM a JOIN b ON a.k=b.k",
        )
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Integer(3),
            Value::Integer(3),
            Value::Integer(6)
        ]]
    );
}

/// A `DISTINCT` aggregate (and `min`/`max`) over a column with a non-BINARY
/// *declared* collation now runs on the VDBE, folding under that collation
/// (`AggStep.collation`): `'a'=='A'` under NOCASE, so `count(DISTINCT x)` is 2.
#[test]
fn distinct_aggregate_over_nocase_runs_on_vdbe() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(x TEXT COLLATE NOCASE)").unwrap();
    c.execute("INSERT INTO u VALUES ('a'),('A'),('b')").unwrap();
    // The declared-collation aggregate runs on the VDBE and dedups under NOCASE.
    let r = c.query_vdbe("SELECT count(DISTINCT x) FROM u").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(2)]]);
    // min/max fold under NOCASE too.
    let r = c.query_vdbe("SELECT min(x), max(x) FROM u").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::Text("a".into()), Value::Text("b".into())]]
    );
}

/// An *explicit* `COLLATE` on a `DISTINCT` aggregate argument
/// (`count(DISTINCT a COLLATE NOCASE)`) drives the dedup, and now folds under that
/// collation on the VDBE (`AggStep.collation`/`AggSpec.collation`): `'a'=='A'`
/// under NOCASE, so `count(DISTINCT)` is 2 and `group_concat(DISTINCT)` is `'a,b'`.
/// An explicit `COLLATE BINARY` keeps `'a'` and `'A'` distinct.
#[test]
fn distinct_aggregate_with_explicit_collate_arg_runs_on_vdbe() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT)").unwrap();
    c.execute("INSERT INTO t VALUES ('a'),('A'),('b')").unwrap();

    for (q, want) in [
        ("SELECT count(DISTINCT a COLLATE NOCASE) FROM t", 2),
        ("SELECT count(DISTINCT (a COLLATE NOCASE)) FROM t", 2),
        ("SELECT count(DISTINCT a COLLATE BINARY) FROM t", 3),
    ] {
        let r = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE to run {q}: {e}"));
        assert_eq!(r.rows, vec![vec![Value::Integer(want)]], "on {q}");
    }
    let r = c
        .query_vdbe("SELECT group_concat(DISTINCT a COLLATE NOCASE) FROM t")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Text("a,b".into())]]);
}

/// The same explicit-`COLLATE` deferral over `GROUP BY`: the tree-walker dedups
/// each group under the argument collation, matching sqlite 3.50.4.
#[test]
fn distinct_aggregate_with_explicit_collate_over_group_by_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(g INT, a TEXT)").unwrap();
    c.execute("INSERT INTO t VALUES (1,'a'),(1,'A'),(1,'b'),(2,'x'),(2,'X')")
        .unwrap();
    let q = "SELECT g, count(DISTINCT a COLLATE NOCASE) FROM t GROUP BY g ORDER BY g";
    let got: Vec<Vec<String>> = c
        .query(q)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.iter().map(render).collect())
        .collect();
    let setup = "CREATE TABLE t(g INT, a TEXT);\nINSERT INTO t VALUES (1,'a'),(1,'A'),(1,'b'),(2,'x'),(2,'X');\n";
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg("-ascii")
        .arg(format!("{setup}{q};"))
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let expected: Vec<Vec<String>> = text
        .split('\u{1e}')
        .filter(|r| !r.is_empty())
        .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
        .collect();
    assert_eq!(got, expected);
}

/// `min`/`max` compare under the argument collation. An explicit `COLLATE` on a
/// `min`/`max` argument now folds under that collation on the VDBE; an explicit
/// `COLLATE BINARY` uses the default comparison.
#[test]
fn min_max_with_explicit_collate_arg_runs_on_vdbe() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT)").unwrap();
    c.execute("INSERT INTO t VALUES ('B'),('a'),('C')").unwrap();

    // NOCASE order is a < B < C, so min='a', max='C' — now on the VDBE.
    let r = c
        .query_vdbe("SELECT min(a COLLATE NOCASE), max(a COLLATE NOCASE) FROM t")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::Text("a".into()), Value::Text("C".into())]],
    );

    // Explicit COLLATE BINARY orders uppercase before lowercase, so min='B'.
    let r = c.query_vdbe("SELECT min(a COLLATE BINARY) FROM t").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Text("B".into())]]);

    // A collation-insensitive aggregate keeps running on the VDBE even with the
    // explicit COLLATE on its argument (count ignores collation).
    let r = c
        .query_vdbe("SELECT count(a COLLATE NOCASE) FROM t")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(3)]]);
}

/// The same `min`/`max` explicit-`COLLATE` deferral over `GROUP BY`, matching
/// sqlite 3.50.4.
#[test]
fn min_max_with_explicit_collate_over_group_by_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(g INT, a TEXT)").unwrap();
    c.execute("INSERT INTO t VALUES (1,'B'),(1,'a'),(1,'C'),(2,'Z'),(2,'y')")
        .unwrap();
    let q = "SELECT g, min(a COLLATE NOCASE), max(a COLLATE NOCASE) FROM t GROUP BY g ORDER BY g";
    let got: Vec<Vec<String>> = c
        .query(q)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.iter().map(render).collect())
        .collect();
    let setup = "CREATE TABLE t(g INT, a TEXT);\nINSERT INTO t VALUES (1,'B'),(1,'a'),(1,'C'),(2,'Z'),(2,'y');\n";
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg("-ascii")
        .arg(format!("{setup}{q};"))
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let expected: Vec<Vec<String>> = text
        .split('\u{1e}')
        .filter(|r| !r.is_empty())
        .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
        .collect();
    assert_eq!(got, expected);
}
