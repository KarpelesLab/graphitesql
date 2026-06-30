//! A comma join with an *unqualified* equi-join predicate in the `WHERE` clause
//! (`FROM t, u WHERE a = x`) is promoted to an explicit join `ON` so the inner
//! table is reached by an index seek, exactly as the explicit-join spelling
//! (`FROM t JOIN u ON a = x`) already was. Previously only a *qualified* predicate
//! (`WHERE t.a = u.x`) promoted, so the unqualified form ran — and rendered — as a
//! full nested-loop `SCAN` of the inner table, diverging from SQLite.
//!
//! The promotion resolves each bare column to its owning source by name (the unique
//! table holding a column of that name); an ambiguous or unknown name declines, so
//! SQLite's own "ambiguous column" error still fires where it should. Because the
//! equality stays in the `WHERE` clause, the promotion never changes results — only
//! the access path (and thus the plan).
//!
//! Verified byte-exact vs sqlite3 3.50.4 (plan and rows) for the two-table cases
//! where the written `FROM` order already matches SQLite's chosen order: a secondary
//! index seek (`SEARCH … USING INDEX`), an INTEGER PRIMARY KEY rowid seek, the
//! reversed (`x = a`) and mixed qualified/unqualified (`a = u.x`) spellings, and
//! aliased tables. Deliberately out of scope (a different optimizer capability, not
//! this promotion): SQLite's cost-based *join reordering* for 3+ tables, and a
//! *range* (`a > x`) join predicate — both still nested-loop in graphite, with
//! correct rows.

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

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

// `u.x` carries a secondary index; `SELECT *` needs `u.y` too, so the seek is a
// plain (non-covering) `USING INDEX`.
const D: &str = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(4,5,6); \
    CREATE TABLE u(x,y); CREATE INDEX ux ON u(x); INSERT INTO u VALUES(1,10),(4,40),(9,90);";
// `u.x` is the rowid (INTEGER PRIMARY KEY).
const DPK: &str = "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(4,5); \
    CREATE TABLE u(x INTEGER PRIMARY KEY,y); INSERT INTO u VALUES(1,10),(4,40);";

/// An unqualified comma-join equality now seeks the inner table by its index —
/// byte-exact plan and rows vs SQLite.
#[test]
fn comma_join_unqualified_promotes_to_index_seek() {
    if !have_sqlite() {
        return;
    }
    // The canonical shape: scan the outer table, seek the inner by index.
    assert_eq!(
        g_eqp(D, "SELECT * FROM t,u WHERE a=x"),
        "SCAN t | SEARCH u USING INDEX ux (x=?)"
    );
    assert_eq!(
        g_eqp(DPK, "SELECT * FROM t,u WHERE a=x"),
        "SCAN t | SEARCH u USING INTEGER PRIMARY KEY (rowid=?)"
    );
    for q in [
        // Unqualified, either operand order.
        "SELECT * FROM t,u WHERE a=x",
        "SELECT * FROM t,u WHERE x=a",
        // Mixed qualified / unqualified operands.
        "SELECT * FROM t,u WHERE a=u.x",
        "SELECT * FROM t,u WHERE u.x=a",
        // An extra non-join filter rides along.
        "SELECT * FROM t,u WHERE a=x AND y>0",
        // Only an inner column projected (still non-covering: y not in the index).
        "SELECT u.y FROM t,u WHERE a=x",
        // Aliased outer / inner tables.
        "SELECT * FROM t AS tt,u WHERE tt.a=x",
        "SELECT * FROM t,u uu WHERE a=uu.x",
    ] {
        assert!(
            g_eqp(D, q).contains("SEARCH u"),
            "expected an index seek on u for {q}, got {}",
            g_eqp(D, q)
        );
        check(D, q);
    }
    for q in ["SELECT * FROM t,u WHERE a=x", "SELECT * FROM t,u WHERE x=a"] {
        check(DPK, q);
    }
}

/// The qualified form still promotes (no regression); shapes the promotion does not
/// cover keep correct rows; an ambiguous bare column is rejected rather than
/// mis-promoted.
#[test]
fn comma_join_promotion_regressions_and_declines() {
    if !have_sqlite() {
        return;
    }
    // Qualified equality — the pre-existing path, unchanged.
    check(D, "SELECT * FROM t,u WHERE t.a=u.x");
    // Not an equality / disjunction: no promotion, rows still correct.
    for q in [
        "SELECT * FROM t,u WHERE a>x",
        "SELECT * FROM t,u WHERE a=x OR b=y",
    ] {
        assert_eq!(g_rows(D, q), sqlite_rows(D, q), "rows diverged for {q}");
    }
    // An ambiguous bare column (`x` lives in both u and w) does not resolve, so the
    // promotion declines and the ambiguity is reported — graphite errors just as
    // SQLite does.
    let amb = "CREATE TABLE t(a,b); CREATE TABLE u(x,m); CREATE TABLE w(x,n);";
    let mut c = Connection::open_memory().unwrap();
    for stmt in amb.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let err = c.query("SELECT a FROM t,u,w WHERE a=x").unwrap_err();
    assert!(
        err.to_string().contains("ambiguous column name: x"),
        "expected an ambiguous-column error, got {err}"
    );
}
