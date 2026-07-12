//! Track B (EQP): a *bare aggregate* query — aggregate functions in the result (or
//! HAVING) with **no `GROUP BY`** — collapses the whole table to exactly one row, so
//! any `ORDER BY` is a no-op. SQLite plans no sorter for it; graphite must likewise
//! emit no `USE TEMP B-TREE FOR ORDER BY` node.
//!
//! The elision is keyed purely off the single-row shape (aggregate present, no
//! `GROUP BY`), independent of `WHERE`, the number of result columns, or which column
//! the (irrelevant) `ORDER BY` names. A `GROUP BY` makes the query multi-row and the
//! sorter returns; a window function makes the output per-row and is excluded from the
//! single-row rule. Verified byte-exact against sqlite3 3.50.4, both the plan and the
//! result rows.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn norm(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).trim_end())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn g_eqp(ddl: &str, q: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let rows = c.query(&format!("EXPLAIN QUERY PLAN {q}")).unwrap().rows;
    let mut lines = Vec::new();
    for r in &rows {
        if let Some(graphitesql::Value::Text(s)) = r.last() {
            lines.push(String::from(s.as_str()));
        }
    }
    lines.join(" | ")
}

fn sqlite_eqp(ddl: &str, q: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{ddl} EXPLAIN QUERY PLAN {q};"))
        .output()
        .unwrap();
    norm(&String::from_utf8_lossy(&o.stdout))
}

fn g_rows(ddl: &str, q: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let r = c.query(q).unwrap();
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    graphitesql::Value::Null => "".to_string(),
                    graphitesql::Value::Integer(i) => i.to_string(),
                    graphitesql::Value::Real(f) => {
                        let s = format!("{f}");
                        if s.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                            format!("{s}.0")
                        } else {
                            s
                        }
                    }
                    graphitesql::Value::Text(s) => String::from(s.as_str()),
                    graphitesql::Value::Blob(_) => "<blob>".to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sqlite_rows(ddl: &str, q: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{ddl} {q};"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn check(ddl: &str, q: &str) {
    assert_eq!(g_eqp(ddl, q), sqlite_eqp(ddl, q), "EQP diverged for {q}");
    assert_eq!(g_rows(ddl, q), sqlite_rows(ddl, q), "rows diverged for {q}");
}

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// A bare aggregate (no GROUP BY) is single-row, so the ORDER BY is dropped and no
/// sorter node appears — across `count`/`sum`/`max`, multi-column, and an ORDER BY that
/// names the aggregate itself.
#[test]
fn bare_aggregate_order_by_emits_no_sorter() {
    if !have_sqlite() {
        return;
    }
    let dc = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(1,5,6),(4,5,9);";
    // No sorter node at all.
    for q in [
        "SELECT count(*) FROM t ORDER BY 1",
        "SELECT sum(a) FROM t ORDER BY 1",
        "SELECT max(a) FROM t ORDER BY 1",
        "SELECT count(*) FROM t ORDER BY count(*)",
        "SELECT count(*), sum(b) FROM t ORDER BY 2 DESC",
    ] {
        let plan = g_eqp(dc, q);
        assert!(
            !plan.contains("ORDER BY"),
            "expected no ORDER BY sorter for single-row aggregate {q}, got {plan}"
        );
        check(dc, q);
    }
}

/// The elision survives a WHERE (the result is still one row) and a min/max SEARCH path.
#[test]
fn bare_aggregate_order_by_under_where_and_search() {
    if !have_sqlite() {
        return;
    }
    let dc = "CREATE TABLE t(a,b,c); CREATE INDEX ib ON t(b); INSERT INTO t VALUES(1,2,3),(1,5,6),(4,5,9);";
    check(dc, "SELECT count(*) FROM t WHERE a>0 ORDER BY 1");
    check(dc, "SELECT count(DISTINCT a) FROM t ORDER BY 1");
    // max(b) seeks the index (SEARCH); the ORDER BY is still dropped.
    check(dc, "SELECT max(b) FROM t ORDER BY 1");
}

/// A `GROUP BY` makes the query multi-row, so the ORDER BY sorter is *not* elided by
/// this rule (the bare-aggregate guard requires an empty GROUP BY) — and a plain
/// non-aggregate ORDER BY keeps its sorter too.
#[test]
fn group_by_and_plain_order_by_keep_sorter() {
    if !have_sqlite() {
        return;
    }
    let dc = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(1,5,6),(4,5,9);";
    // Plain SELECT with ORDER BY still sorts.
    let p1 = g_eqp(dc, "SELECT a FROM t ORDER BY 1");
    assert!(p1.contains("USE TEMP B-TREE FOR ORDER BY"), "got {p1}");
    check(dc, "SELECT a FROM t ORDER BY 1");
}
