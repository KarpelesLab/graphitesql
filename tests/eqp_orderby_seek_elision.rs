//! Track B (EQP): a `WHERE` seek that pins a column to a single value lets
//! sqlite drop the matching `ORDER BY` — there is nothing left to sort along
//! that column. Two cases graphite previously planned with a needless
//! `USE TEMP B-TREE FOR ORDER BY`:
//!
//!  * A bare rowid / INTEGER PRIMARY KEY equality (`id = 5`, or a one-element
//!    `IN`) returns at most one row, so *any* `ORDER BY` is already satisfied —
//!    regardless of which columns it names.
//!  * A `col IS NULL` conjunct pins `col` to the (single) NULL key, exactly like
//!    a value equality, so an `ORDER BY` term on that column is constant and
//!    drops out; a remaining term on an index-walked column keeps the seek's
//!    order.
//!
//! Both now feed `order_satisfied_by_scan` (`rowid_eq_single_row` and the
//! `IS NULL`-as-constant handling in `seek_order_prefix`), so the sort is elided
//! and the EQP matches sqlite. Verified byte-exact against sqlite3 3.50.4, both
//! the plan and the result rows (row order matters — eliding the sort means the
//! seek order must agree).

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
            lines.push(s.clone());
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
                    graphitesql::Value::Real(f) => format!("{f}"),
                    graphitesql::Value::Text(s) => s.clone(),
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

#[test]
fn single_row_rowid_eq_elides_order_by() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // A `rowid`/IPK equality matches one row, so every ORDER BY shape — on the
    // key, on another column, descending, multi-term — needs no temp b-tree.
    let d = "CREATE TABLE t(id INTEGER PRIMARY KEY, b, c); CREATE INDEX ib ON t(b); \
             INSERT INTO t VALUES(5,1,9),(2,3,8),(7,1,4);";
    for q in [
        "SELECT * FROM t WHERE id = 5 ORDER BY id",
        "SELECT * FROM t WHERE id = 5 ORDER BY b",
        "SELECT * FROM t WHERE id = 5 ORDER BY id DESC",
        "SELECT * FROM t WHERE id = 5 ORDER BY b, c",
        "SELECT * FROM t WHERE id = 5 ORDER BY c DESC",
        // A one-element IN is still a single rowid.
        "SELECT * FROM t WHERE id IN (5) ORDER BY b",
        // The `rowid` alias works the same as the named IPK.
        "SELECT c FROM t WHERE rowid = 2 ORDER BY b",
        // Guard: a multi-value IN is several rows — the plans must still agree
        // (neither elides a non-rowid ORDER BY here).
        "SELECT * FROM t WHERE id IN (5,7) ORDER BY b",
    ] {
        check(d, q);
    }
}

#[test]
fn isnull_constant_elides_order_by() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // `a IS NULL` pins `a` to its single NULL key: an ORDER BY on `a` is constant
    // and drops, while a following term on an index-walked column keeps the seek
    // order. Single-column index: only `a` is walked.
    let d1 = "CREATE TABLE t(a, b, c); CREATE INDEX ia ON t(a); \
              INSERT INTO t VALUES(NULL,30,1),(NULL,10,2),(2,3,3),(NULL,20,4);";
    for q in [
        "SELECT b FROM t WHERE a IS NULL ORDER BY a",
        "SELECT b FROM t WHERE a IS NULL ORDER BY a DESC",
    ] {
        check(d1, q);
    }

    // Composite index (a, b): with `a` pinned NULL the walk orders by `b`, so a
    // leading constant `a` term plus a `b` term (in either position) elides.
    let d2 = "CREATE TABLE t(a, b, c); CREATE INDEX iab ON t(a, b); \
              INSERT INTO t VALUES(NULL,30,1),(NULL,10,2),(2,3,3),(NULL,20,4);";
    for q in [
        "SELECT c FROM t WHERE a IS NULL ORDER BY a, b",
        "SELECT c FROM t WHERE a IS NULL ORDER BY b",
        // Both columns pinned constant (`a` NULL, `b` equality) — order vacuous.
        "SELECT c FROM t WHERE a = 2 AND b IS NULL ORDER BY a, b",
        "SELECT c FROM t WHERE a = 2 AND b IS NULL ORDER BY b, a",
    ] {
        check(d2, q);
    }
}
