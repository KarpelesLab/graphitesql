//! A recursive CTE whose non-recursive anchor reads from a real table (or whose
//! recursive arm joins one) used to crash graphite with a stack overflow before
//! producing any rows. The fault was in column-origin resolution, not the
//! recursion itself: `named_source_origins_in` resolved a `FROM c` reference by
//! descending into the CTE body with `c` still in scope, so the body's own
//! self-reference re-entered the same path without end. The overflow only fired
//! when the body carried a base-table source to resolve origins for — a
//! `SELECT <const>` anchor sidestepped it — so it lurked behind the textbook
//! counter example.
//!
//! The fix makes a recursive CTE body resolve to the conservative `None` origin
//! (and drops the CTE from its own body's scope), which terminates. These cases
//! now run and match `sqlite3` 3.50.4 row-for-row.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn g_rows(q: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a,b,c)").unwrap();
    c.execute("INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9)")
        .unwrap();
    let r = c.query(q).unwrap();
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    graphitesql::Value::Null => "".to_string(),
                    graphitesql::Value::Integer(i) => i.to_string(),
                    graphitesql::Value::Real(f) => format!("{f}"),
                    graphitesql::Value::Text(s) => String::from(s.as_str()),
                    graphitesql::Value::Blob(_) => "<blob>".to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sqlite_rows(q: &str) -> String {
    let ddl = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);";
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{ddl} {q};"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

/// A base-table anchor (and a base-table join in the recursive arm) terminates and
/// matches SQLite — previously a stack overflow.
#[test]
fn base_table_anchor_terminates_and_matches() {
    let queries = [
        // The original crasher: a multi-row base-table anchor.
        "WITH RECURSIVE c(n) AS (SELECT a FROM t UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT group_concat(n) FROM c",
        // Single-row base-table anchors (aggregate / filtered) crashed too.
        "WITH RECURSIVE c(n) AS (SELECT max(a) FROM t UNION ALL SELECT n+1 FROM c WHERE n<8) \
         SELECT group_concat(n) FROM c",
        "WITH RECURSIVE c(n) AS (SELECT a FROM t WHERE a=1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT group_concat(n) FROM c",
        // A base-table join in the recursive arm exercises the same resolution path.
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT a FROM c JOIN t ON t.a=c.x+2 WHERE a<8) \
         SELECT group_concat(x) FROM c",
        // The const-anchor textbook form keeps working (the path that never crashed).
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<5) \
         SELECT group_concat(n) FROM c",
        // `UNION` (distinct) variant of the base-table anchor.
        "WITH RECURSIVE c(n) AS (SELECT a FROM t UNION SELECT n+1 FROM c WHERE n<5) \
         SELECT group_concat(n) FROM c",
    ];
    if have_sqlite() {
        for q in queries {
            assert_eq!(g_rows(q), sqlite_rows(q), "rows diverged for {q}");
        }
    } else {
        // Even without the oracle, the queries must terminate (no overflow/panic).
        for q in queries {
            let _ = g_rows(q);
        }
    }
}
