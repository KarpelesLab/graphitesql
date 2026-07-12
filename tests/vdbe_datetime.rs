//! The date/time scalar functions (`date`, `time`, `datetime`, `julianday`,
//! `unixepoch`, `strftime`, `timediff`) operate purely on their argument
//! *values* — each dispatches to `datetime::<fn>(&values)` without touching the
//! evaluation `ctx` — so they compile to `Op::Func` and run on the VDBE rather
//! than falling back to the tree-walker. The result is byte-identical to the
//! tree-walker / sqlite3 because both paths call the same `datetime` routine on
//! the same reconstructed argument values.
//!
//! Every query below uses an explicit time value plus deterministic modifiers
//! (never `'now'` or a no-time-value default form), so the comparison is stable.
//! `query_vdbe` errors on any fallback, so a passing query proves the call
//! compiled. Results match the tree-walker and the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

// `a` is a plain (unindexed) integer so `ORDER BY a` is served by the VDBE
// sorter rather than an index/rowid scan (the latter defers to the tree-walker).
const SETUP: &str = "\
    CREATE TABLE t(a INTEGER, d TEXT, u INTEGER);\n\
    INSERT INTO t VALUES\n\
      (1,'2020-01-01',1577836800),\n\
      (2,'2021-06-15 12:30:00',1623760200),\n\
      (3,'1999-12-31',946598400),\n\
      (4,NULL,NULL);\n";

const QUERIES: &[&str] = &[
    // date() on a stored text date.
    "SELECT a, date(d) FROM t ORDER BY a",
    // time() on a stored datetime.
    "SELECT a, time(d) FROM t ORDER BY a",
    // datetime() with a '+N days' modifier.
    "SELECT a, datetime(d, '+1 day') FROM t ORDER BY a",
    // datetime() with a start-of-month modifier chain.
    "SELECT a, datetime(d, 'start of month', '+15 days') FROM t ORDER BY a",
    // julianday() on a text date (REAL result).
    "SELECT a, julianday(d) FROM t ORDER BY a",
    // unixepoch() on a text date.
    "SELECT a, unixepoch(d) FROM t ORDER BY a",
    // datetime() converting a stored unix timestamp back to a calendar string.
    "SELECT a, datetime(u, 'unixepoch') FROM t ORDER BY a",
    // strftime() with several conversions.
    "SELECT a, strftime('%Y/%m/%d %H:%M', d) FROM t ORDER BY a",
    // strftime() day-of-week and day-of-year.
    "SELECT a, strftime('%w-%j', d) FROM t ORDER BY a",
    // timediff() between two explicit instants.
    "SELECT a, timediff(d, '2020-01-01') FROM t ORDER BY a",
    // In a WHERE predicate (the call gates the row).
    "SELECT a FROM t WHERE strftime('%Y', d) = '2020' ORDER BY a",
    // A julianday() difference used in arithmetic.
    "SELECT a, julianday(d) - julianday('2020-01-01') FROM t ORDER BY a",
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
fn datetime_runs_on_vdbe_and_matches_tree_walker() {
    let c = conn();
    for q in QUERIES {
        // `query_vdbe` errors on fallback, so this proves the call compiled.
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn datetime_matches_sqlite3() {
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
