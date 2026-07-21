//! A CTE column that is a direct column reference inherits its base column's
//! affinity AND collation, exactly like a derived subquery and matching SQLite.
//!
//! Before the fix, `cte_columns` hardcoded `BLOB`/`BINARY` for every CTE column,
//! so `WITH t AS (SELECT a FROM base) SELECT DISTINCT a FROM t` deduped under
//! BINARY even when `base.a` is `COLLATE NOCASE` — giving `A,a,B` instead of
//! SQLite's `A,B`. The equivalent derived-subquery form already worked (it uses
//! `subquery_column_origins`); the CTE path now resolves the same origins.
//!
//! Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE base(a TEXT COLLATE NOCASE, k INTEGER);\n\
    INSERT INTO base VALUES('A',1),('a',2),('B',3),('a',4);\n";

// Each query exercises the CTE column's inherited collation (NOCASE on `base.a`)
// or affinity (INTEGER on `base.k`). All would differ under the old BLOB/BINARY
// hardcode.
const QUERIES: &[&str] = &[
    // DISTINCT dedups under the inherited NOCASE collation.
    "WITH t AS (SELECT a FROM base) SELECT DISTINCT a FROM t ORDER BY a",
    // count(DISTINCT ...) likewise folds A/a together.
    "WITH t AS (SELECT a FROM base) SELECT count(DISTINCT a) FROM t",
    // GROUP BY groups case-insensitively.
    "WITH t AS (SELECT a FROM base) SELECT a, count(*) FROM t GROUP BY a ORDER BY a",
    // ORDER BY over the CTE column keeps the base collation's ordering.
    "WITH t AS (SELECT a FROM base) SELECT a FROM t ORDER BY a",
    // An explicit column list still inherits the origin.
    "WITH t(x) AS (SELECT a FROM base) SELECT DISTINCT x FROM t ORDER BY x",
    // A chained (sibling) CTE carries the collation through each level.
    "WITH u AS (SELECT a FROM base), v AS (SELECT a FROM u) SELECT DISTINCT a FROM v ORDER BY a",
    // Integer affinity is inherited too: a text bound is coerced for the compare.
    "WITH t AS (SELECT k FROM base) SELECT k FROM t WHERE k < '10' ORDER BY k",
];

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => String::from(s.as_str()),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

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

#[test]
fn cte_column_collation_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    for q in QUERIES {
        let got: Vec<Vec<String>> = c
            .query(q)
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
        assert_eq!(got, want, "graphite vs sqlite3 diverged on {q}");
    }
}
