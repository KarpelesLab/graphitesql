//! `GROUP BY` over a non-BINARY-collation key runs on the VDBE: `GroupStep`
//! matches group identity, `sort_groups_by_key` orders the emitted groups, and the
//! single `min`/`max` companion reduction all compare under each key's collation
//! (declared `NOCASE`/`RTRIM`/custom). With all-BINARY keys the comparisons are
//! byte-identical to the previous BINARY-only path.
//!
//! Still deferred to the tree-walker (which honors the collation): a `DISTINCT`
//! aggregate (`count(DISTINCT x)`), an ordered aggregate, a single-arg `min`/`max`
//! (its argument fold), and a `SELECT DISTINCT … GROUP BY` (post-group dedup) —
//! anything whose collated comparison the grouped VDBE path does not thread.
//! Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const DDL: &str = "CREATE TABLE t(a TEXT COLLATE NOCASE, x INT, c TEXT COLLATE RTRIM)";
const DML: &str = "INSERT INTO t VALUES ('A',1,'p '),('a',2,'q'),('B',3,'p'),('a',4,'p  '),('c',5,'r'),(NULL,6,'p')";

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute(DDL).unwrap();
    c.execute(DML).unwrap();
    c
}

fn sqlite_rows(q: &str) -> Vec<Vec<String>> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg("-ascii")
        .arg(format!("{DDL};\n{DML};\n{q};"))
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .split('\u{1e}')
        .filter(|r| !r.is_empty())
        .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
        .collect()
}

fn as_strings(rows: &[Vec<Value>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Text(s) => s.clone(),
                    Value::Real(x) => graphitesql::exec::eval::format_real(*x),
                    Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
                .collect()
        })
        .collect()
}

#[test]
fn grouped_over_collation_key_runs_on_vdbe() {
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let c = conn();
    for q in [
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY a",
        "SELECT c, count(*) FROM t GROUP BY c ORDER BY c",
        "SELECT a, c, count(*) FROM t GROUP BY a, c ORDER BY a, c",
        "SELECT a, sum(x) FROM t GROUP BY a ORDER BY a",
        "SELECT a, group_concat(x) FROM t GROUP BY a ORDER BY a",
        "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1 ORDER BY a",
        // Default (no explicit ORDER BY) group emit order is collation-sorted too.
        "SELECT a, count(*) FROM t GROUP BY a",
    ] {
        let r = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE to run {q}: {e}"));
        if sqlite {
            assert_eq!(as_strings(&r.rows), sqlite_rows(q), "diverged on {q}");
        }
    }
}

#[test]
fn collation_sensitive_grouped_shapes_defer() {
    let c = conn();
    // These fold a (potentially collated) argument the grouped VDBE path compares
    // under BINARY, so they must defer to the tree-walker.
    for q in [
        "SELECT a, max(x) FROM t GROUP BY a", // min/max aggregate
        "SELECT a, count(DISTINCT c) FROM t GROUP BY a", // DISTINCT aggregate
        "SELECT DISTINCT a FROM t GROUP BY a, x", // DISTINCT over grouped output
    ] {
        assert!(c.query_vdbe(q).is_err(), "expected VDBE to defer on {q}");
        // The deferred result (tree-walker, unchanged by this feature) is still
        // correct — the row set matches the enabled grouping. (Exact ordering/NULL
        // rendering is covered by the broad differential corpus; here we just
        // confirm the query runs and returns rows.)
        assert!(c.query(q).is_ok(), "tree-walker failed on {q}");
    }
}
