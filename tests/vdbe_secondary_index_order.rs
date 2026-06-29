//! Track B (VDBE row order): a secondary-index seek returns rows in *index-key*
//! order, which a plain rowid-order table scan does not reproduce. The VDBE
//! executes a `WHERE`-bearing single-table `SELECT` as a rowid scan, so for a
//! query SQLite answers by walking an index — a range bound, a multi-value `IN`,
//! or a covering `IS NOT NULL` — the VDBE's output order would diverge whenever
//! there is no `ORDER BY` to re-sort.
//!
//! `run_select_vdbe` now defers exactly those queries to the tree-walker (via
//! `vdbe_seek_returns_index_order`), whose seek paths walk the index in key order
//! — matching SQLite byte-for-byte. Single-key seeks (`a=?`, `a IS NULL`, a
//! one-element `IN`) keep rowid order and stay on the VDBE; an explicit `ORDER BY`
//! makes the order independent of the access path, so those stay too.
//!
//! The covering `IS NOT NULL` seek is also a new plan (`try_isnotnull_covering`):
//! `SELECT a … WHERE a IS NOT NULL` over an index on `a` reads `USING COVERING
//! INDEX a (a>?)`, while the near-full-table non-covering `SELECT *` stays a
//! `SCAN` on both sides. Every case is checked against sqlite3 3.50.4, both the
//! plan and the result rows (row order is the whole point here).

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
fn range_seek_rows_in_index_order() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // `a` is indexed but stored out of rowid order, so an index walk and a rowid
    // scan give different orders — the VDBE must defer to keep index order.
    let d = "CREATE TABLE t(a, b, c); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(1,2,3),(NULL,3,4),(5,5,6),(NULL,7,8),(2,9,1);";
    for q in [
        // Covering and non-covering range seeks, single- and multi-row.
        "SELECT a FROM t WHERE a > 1",
        "SELECT b FROM t WHERE a > 1",
        "SELECT * FROM t WHERE a > 1",
        "SELECT a FROM t WHERE a >= 2",
        "SELECT a FROM t WHERE a BETWEEN 1 AND 5",
        "SELECT a FROM t WHERE a < 5",
        // A LIMIT without ORDER BY exposes which row comes *first* — it must be
        // the first in index order, not rowid order.
        "SELECT a FROM t WHERE a > 1 LIMIT 1",
        // An explicit ORDER BY fixes the order regardless of the access path —
        // these stay on the VDBE and still match.
        "SELECT a FROM t WHERE a > 1 ORDER BY a DESC",
    ] {
        check(d, q);
    }
}

#[test]
fn single_key_seeks_keep_rowid_order() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // A single index key (an equality or a one-element `IN`) returns its rows in
    // rowid order — exactly the VDBE scan order — so these stay on the VDBE and
    // still match. (A *multi-value* `IN` is a separate, pre-existing ordering gap:
    // SQLite seeks in sorted value order while the tree-walker walks the list in
    // written order, so neither engine reproduces it — it is intentionally left
    // untouched here.)
    let d = "CREATE TABLE t(a, b); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(5,1),(2,2),(8,3),(2,4),(5,5);";
    for q in [
        "SELECT a, b FROM t WHERE a IN (5)",
        "SELECT a, b FROM t WHERE a = 2",
        "SELECT a, b FROM t WHERE a = 5",
    ] {
        check(d, q);
    }
}

#[test]
fn covering_isnotnull_seeks_the_index() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // `col IS NOT NULL` selects every non-NULL key: SQLite reads a covering index
    // as `col>?` (NULLs sort first). The near-full-table non-covering `SELECT *`
    // loses to a plain `SCAN` on both sides; `count(*)`/`sum` are covering too.
    let d = "CREATE TABLE t(a, b, c); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(1,2,3),(NULL,3,4),(5,5,6),(NULL,7,8),(2,9,1);";
    for q in [
        "SELECT a FROM t WHERE a IS NOT NULL",
        "SELECT count(*) FROM t WHERE a IS NOT NULL",
        "SELECT sum(a) FROM t WHERE a IS NOT NULL",
        "SELECT a, count(*) FROM t WHERE a IS NOT NULL GROUP BY a",
        // Non-covering: both SCAN (regression guard — must not start seeking).
        "SELECT * FROM t WHERE a IS NOT NULL",
        "SELECT b FROM t WHERE a IS NOT NULL",
    ] {
        check(d, q);
    }
}

#[test]
fn eq_prefix_then_range_in_index_order() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // A composite index seeked with an equality prefix and a range on the next
    // column walks that column's order, distinct from rowid order.
    let d = "CREATE TABLE t(a, b, c); CREATE INDEX iab ON t(a, b); \
             INSERT INTO t VALUES(1,30,1),(1,10,2),(1,20,3),(2,5,4),(1,40,5);";
    for q in [
        "SELECT b FROM t WHERE a = 1 AND b > 10",
        "SELECT c FROM t WHERE a = 1 AND b >= 20",
        // Single key (equality on the whole prefix) keeps rowid order.
        "SELECT c FROM t WHERE a = 1 AND b = 30",
    ] {
        check(d, q);
    }
}

#[test]
fn rowid_range_keeps_rowid_order() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // A range on the INTEGER PRIMARY KEY walks the table b-tree in rowid order —
    // the same order the VDBE scan produces — so it stays on the VDBE and matches.
    let d = "CREATE TABLE t(id INTEGER PRIMARY KEY, a); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(5,1),(2,3),(7,1),(1,9);";
    for q in [
        "SELECT id FROM t WHERE id > 1",
        "SELECT id, a FROM t WHERE id BETWEEN 2 AND 7",
    ] {
        check(d, q);
    }
}
