//! Track B: ordered `group_concat(x ORDER BY …)` runs on the VDBE — on the bare
//! single-table path, over `GROUP BY`, and over a two-table join. The collected
//! values record their `ORDER BY` key rows so finalization sorts them (stable,
//! BINARY collation, SQLite NULL placement) before concatenating. `query_vdbe`
//! errors on any fallback, so these passing proves the VDBE compiled them;
//! results match the tree-walker and sqlite 3.50.4. `DISTINCT` + `ORDER BY` and
//! `ORDER BY` on a non-`group_concat` aggregate intentionally still defer.

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
     INSERT INTO t(a,b,s) VALUES (1,3,'c'),(1,1,'a'),(1,2,'b'),(2,5,'z'),(2,4,'y'),(1,NULL,'d');\n";

const QUERIES: &[&str] = &[
    // ORDER BY another column, ascending and descending.
    "SELECT group_concat(s ORDER BY b) FROM t",
    "SELECT group_concat(s ORDER BY b DESC) FROM t",
    // ORDER BY the aggregated column itself.
    "SELECT group_concat(s ORDER BY s) FROM t",
    "SELECT group_concat(s ORDER BY s DESC) FROM t",
    // Multiple ORDER BY keys.
    "SELECT group_concat(s ORDER BY a, b) FROM t",
    "SELECT group_concat(s ORDER BY a DESC, s) FROM t",
    // ORDER BY a key with NULLs (default placement + explicit NULLS FIRST/LAST).
    "SELECT group_concat(s ORDER BY b) FROM t",
    "SELECT group_concat(s ORDER BY b NULLS FIRST) FROM t",
    "SELECT group_concat(s ORDER BY b DESC NULLS LAST) FROM t",
    // ORDER BY combined with a FILTER.
    "SELECT group_concat(s ORDER BY b) FILTER (WHERE b IS NOT NULL) FROM t",
    // ORDER BY by a computed expression.
    "SELECT group_concat(s ORDER BY -b) FROM t",
];

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT, s TEXT)")
        .unwrap();
    c.execute("INSERT INTO t(a,b,s) VALUES (1,3,'c'),(1,1,'a'),(1,2,'b'),(2,5,'z'),(2,4,'y'),(1,NULL,'d')")
        .unwrap();
    c
}

#[test]
fn ordered_group_concat_runs_on_vdbe_and_match_tree_walker() {
    let c = conn();
    for q in QUERIES {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn ordered_group_concat_match_sqlite3() {
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

// ── Ordered group_concat over GROUP BY ──────────────────────────────────────
const GROUPED: &[&str] = &[
    "SELECT a, group_concat(s ORDER BY b) FROM t GROUP BY a ORDER BY a",
    "SELECT a, group_concat(s ORDER BY b DESC) FROM t GROUP BY a ORDER BY a",
    "SELECT a, group_concat(s ORDER BY s DESC) FROM t GROUP BY a ORDER BY a",
    // Ordered group_concat mixed with plain aggregates in one grouped row.
    "SELECT a, count(*), group_concat(s ORDER BY b) FROM t GROUP BY a ORDER BY a",
    // Ordered group_concat + FILTER per group.
    "SELECT a, group_concat(s ORDER BY b) FILTER (WHERE b IS NOT NULL) FROM t GROUP BY a ORDER BY a",
];

#[test]
fn ordered_group_concat_over_group_by_runs_on_vdbe() {
    let c = conn();
    for q in GROUPED {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn ordered_group_concat_over_group_by_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
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

/// Ordered `group_concat` over a two-table join runs on the VDBE join aggregate
/// path, ordering by a column from either side.
#[test]
fn ordered_group_concat_over_join_runs_on_vdbe() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(id INTEGER PRIMARY KEY, k INT, v TEXT)")
        .unwrap();
    c.execute("CREATE TABLE b(id INTEGER PRIMARY KEY, k INT, w INT)")
        .unwrap();
    c.execute("INSERT INTO a(k,v) VALUES (1,'p'),(1,'q'),(2,'r')")
        .unwrap();
    c.execute("INSERT INTO b(k,w) VALUES (1,30),(1,10),(2,20)")
        .unwrap();
    let q = "SELECT group_concat(a.v ORDER BY b.w) FROM a JOIN b ON a.k = b.k";
    let got = c.query_vdbe(q).unwrap().rows;
    let want = c.query(q).unwrap().rows;
    assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    // Joined rows: (v=p,w=30),(p,10),(q,30),(q,10),(r,20). Ordered by w:
    // 10:p,10:q,20:r,30:p,30:q → "p,q,r,p,q".
    assert_eq!(got, vec![vec![Value::Text("p,q,r,p,q".into())]]);
}

/// `DISTINCT` + `ORDER BY` and an `ORDER BY` on a non-`group_concat` aggregate
/// defer to the tree-walker; the default path still gives the right answer.
#[test]
fn distinct_ordered_and_non_concat_ordered_defer() {
    let c = conn();
    // group_concat(DISTINCT … ORDER BY …) defers (VDBE errors) …
    assert!(
        c.query_vdbe("SELECT group_concat(DISTINCT s ORDER BY s) FROM t")
            .is_err()
    );
    // … and an ORDER BY on a non-concat aggregate defers too.
    assert!(c.query_vdbe("SELECT count(s ORDER BY s) FROM t").is_err());
    // The tree-walker fallback still answers correctly.
    let r = c
        .query("SELECT group_concat(DISTINCT s ORDER BY s) FROM t")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Text("a,b,c,d,y,z".into())]]);
}
