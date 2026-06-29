//! Track B (EQP): a `col IS NULL` conjunct is a *seekable* equality against a
//! NULL index key. SQLite renders `SEARCH … USING [COVERING] INDEX … (col=?)`
//! for it (NULLs sort first in the b-tree, and the seek finds exactly the
//! NULL-keyed entries); graphite previously SCANned because its eq-constraint
//! collector only recognised `col = const`.
//!
//! `collect_isnull_cols` now feeds the index chooser (and `eqp_access`, in
//! lockstep) a NULL key for each `col IS NULL`, so the seek and its EQP label
//! match sqlite. The constraint is tracked apart from value equalities so the
//! rowid / INTEGER PRIMARY KEY fast paths never fire for `rowid IS NULL`
//! (sqlite scans there) and so `col = NULL` (never true) keeps bailing. Every
//! case is verified byte-exact against sqlite3 3.50.4, EQP and result rows.
//!
//! Each DDL uses a single relevant index so the chosen seek index is
//! unambiguous (sqlite's no-stats tiebreak among equal-prefix indexes is
//! creation-order-dependent — a separate, deferred cost-model gap). `IS NULL`
//! on a WITHOUT ROWID secondary index is likewise out of scope (a distinct
//! pre-existing gap): graphite scans it, but the rows stay correct.

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
fn single_column_isnull_seeks_the_index() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    let d = "CREATE TABLE t(a, b, c); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(1,2,3),(NULL,3,4),(1,5,6),(NULL,7,8);";
    for q in [
        // Non-covering seek: reads the table for b/c.
        "SELECT * FROM t WHERE a IS NULL",
        "SELECT b FROM t WHERE a IS NULL",
        // Covering: only `a` (and the rowid) referenced — `USING COVERING INDEX`.
        "SELECT count(*) FROM t WHERE a IS NULL",
        "SELECT sum(a) FROM t WHERE a IS NULL",
        "SELECT a, count(*) FROM t WHERE a IS NULL GROUP BY a",
        // `IS NOT NULL` is not seekable — both scan (regression guard).
        "SELECT * FROM t WHERE a IS NOT NULL",
    ] {
        check(d, q);
    }
}

#[test]
fn composite_index_isnull_prefix() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // A two-column index seeked with a NULL prefix, a NULL/value mix, and a
    // range on the column after a NULL equality prefix.
    let d = "CREATE TABLE t(a, b); CREATE INDEX iab ON t(a, b); \
             INSERT INTO t VALUES(1,2),(NULL,3),(NULL,8),(NULL,NULL),(1,NULL);";
    for q in [
        "SELECT * FROM t WHERE a IS NULL AND b IS NULL",
        "SELECT * FROM t WHERE a IS NULL AND b = 3",
        "SELECT * FROM t WHERE a = 1 AND b IS NULL",
        "SELECT * FROM t WHERE a IS NULL AND b > 2",
        "SELECT * FROM t WHERE a IS NULL AND b BETWEEN 2 AND 8",
        "SELECT count(*) FROM t WHERE a IS NULL AND b IS NULL",
    ] {
        check(d, q);
    }
}

#[test]
fn rowid_isnull_still_scans() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // The INTEGER PRIMARY KEY is never NULL, so `id IS NULL` cannot use the
    // rowid fast path — sqlite (and graphite) SCAN. A secondary `a IS NULL` on
    // the same table still seeks. `a = NULL` is never true and keeps bailing.
    let d = "CREATE TABLE t(id INTEGER PRIMARY KEY, a); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(1,NULL),(2,5),(3,NULL);";
    check(d, "SELECT * FROM t WHERE id IS NULL");
    check(d, "SELECT * FROM t WHERE a IS NULL");
}

#[test]
fn without_rowid_secondary_isnull_seeks() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // A `col IS NULL` on a WITHOUT ROWID table's secondary index seeks the index
    // (its records carry the trailing PRIMARY KEY columns) exactly as on a rowid
    // table. The PRIMARY KEY itself is NOT NULL, so `k IS NULL` cannot match and
    // both scan. (A *composite* covering seek that returns several rows is omitted
    // here: graphite emits them in PK order and sqlite in index order — a separate,
    // pre-existing WITHOUT ROWID ordering quirk unrelated to the IS NULL seek.)
    let d1 = "CREATE TABLE w(k TEXT PRIMARY KEY, v, u) WITHOUT ROWID; \
              CREATE INDEX iv ON w(v); \
              INSERT INTO w VALUES('a',NULL,1),('b',2,2),('c',NULL,3);";
    check(d1, "SELECT * FROM w WHERE v IS NULL");
    check(d1, "SELECT count(*) FROM w WHERE v IS NULL");
    check(d1, "SELECT k FROM w WHERE v IS NULL");
    check(d1, "SELECT * FROM w WHERE k IS NULL");

    // Composite secondary index: a NULL/NULL prefix (empty result) and a
    // NULL-then-value prefix (single row) — both order-insensitive.
    let d2 = "CREATE TABLE w(k TEXT PRIMARY KEY, v, u) WITHOUT ROWID; \
              CREATE INDEX ivu ON w(v, u); \
              INSERT INTO w VALUES('a',NULL,1),('b',2,2),('c',NULL,NULL);";
    check(d2, "SELECT k FROM w WHERE v IS NULL AND u IS NULL");
    check(d2, "SELECT k, u FROM w WHERE v IS NULL AND u = 1");
}
