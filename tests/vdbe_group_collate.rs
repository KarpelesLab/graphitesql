//! `GROUP BY` over a non-BINARY-collation key runs on the VDBE: `GroupStep` matches
//! group identity, `sort_groups_by_key` orders the emitted groups, the `min`/`max`
//! companion reduction, and each aggregate's `DISTINCT` dedup / `min`/`max`
//! reduction (`AggSpec.collation`) all compare under the relevant collation
//! (declared `NOCASE`/`RTRIM`/custom). With all-BINARY collations the comparisons
//! are byte-identical to the previous BINARY-only path.
//!
//! A `SELECT DISTINCT … GROUP BY` now runs on the VDBE too — its post-group dedup
//! resolves each output column's collation into the `DistinctCheck`. A bare
//! aggregate (no GROUP BY) + `DISTINCT` + explicit `COLLATE` also runs (one row, so
//! DISTINCT is a no-op). Verified against sqlite3 3.50.4.

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
                    Value::Text(s) => String::from(s.as_str()),
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
        // An aggregate that folds its argument under a collation — `min`/`max`
        // (the reduction) and `count(DISTINCT …)` (the dedup) — now runs on the VDBE
        // too, honoring the argument's declared collation.
        "SELECT a, max(x) FROM t GROUP BY a ORDER BY a",
        "SELECT a, max(c), min(c) FROM t GROUP BY a ORDER BY a",
        "SELECT a, count(DISTINCT c) FROM t GROUP BY a ORDER BY a",
        // An explicit `COLLATE` on the group key (over a differently-collated column)
        // now runs on the VDBE — `group_key_collations` resolves the override.
        "SELECT c COLLATE NOCASE, count(*) FROM t GROUP BY c COLLATE NOCASE ORDER BY 1",
        "SELECT a COLLATE BINARY, count(*) FROM t GROUP BY a COLLATE BINARY ORDER BY 1",
        // A `SELECT DISTINCT … GROUP BY` post-group dedup honors each output
        // column's collation (`distinct_collations`), so a declared-NOCASE grouped
        // column and an explicit projection `COLLATE` both run on the VDBE. (The `a`
        // queries exclude the NULL row: the `-ascii` harness drops an all-empty
        // single-column row, so it would spuriously differ.)
        "SELECT DISTINCT a FROM t WHERE a IS NOT NULL GROUP BY a, x ORDER BY 1",
        "SELECT DISTINCT a COLLATE NOCASE FROM t WHERE a IS NOT NULL GROUP BY a ORDER BY 1",
        "SELECT DISTINCT c FROM t GROUP BY c, x ORDER BY 1",
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
fn bare_aggregate_distinct_collation_runs_on_vdbe() {
    let c = conn();
    // A *bare aggregate* (no GROUP BY) + DISTINCT + explicit `COLLATE` yields one
    // row, so DISTINCT is a no-op there and its collation is irrelevant — it now runs
    // on the VDBE and matches the tree-walker.
    let q = "SELECT DISTINCT max(c) COLLATE NOCASE FROM t";
    let got = c
        .query_vdbe(q)
        .unwrap_or_else(|e| panic!("expected VDBE to run {q}: {e}"));
    assert_eq!(
        got.rows,
        c.query(q).unwrap().rows,
        "VDBE vs tree-walker on {q}"
    );
}
