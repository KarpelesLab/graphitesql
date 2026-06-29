//! Track B (EQP): an aliased table is named in the query plan by its *alias*
//! alone — `SCAN x`, `SEARCH x USING INDEX …` — exactly as sqlite does, NOT by the
//! `table AS alias` form graphite used to print. The single exception sqlite makes
//! is the bare `count(*)` covering-index optimization, which it labels with the
//! *table name* even when the table is aliased (`SCAN t USING COVERING INDEX i`);
//! graphite mirrors that quirk. Verified byte-exact against sqlite3 3.50.4 across
//! plain scans, index/rowid SEARCHes, covering and ORDER-BY index scans, two- and
//! comma-joins (both sides aliased), and a virtual table.

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

fn check(ddl: &str, q: &str) {
    assert_eq!(g_eqp(ddl, q), sqlite_eqp(ddl, q), "EQP diverged for {q}");
}

#[test]
fn aliased_tables_named_by_alias() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    let d = "CREATE TABLE t(a, b); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(1,2),(3,4),(1,8);";
    for q in [
        // Plain scan, AS and bare-alias spellings.
        "SELECT * FROM t AS x",
        "SELECT * FROM t x",
        // Index SEARCH, qualified predicate through the alias.
        "SELECT * FROM t AS x WHERE x.a = 1",
        "SELECT * FROM t AS x WHERE a = 1",
        // A non-seekable predicate still SCANs, under the alias.
        "SELECT * FROM t AS x WHERE x.b = 2",
        // Covering scan / ORDER-BY index scan keep the alias.
        "SELECT a FROM t AS x",
        "SELECT a FROM t AS x ORDER BY a",
        "SELECT a FROM t AS longalias ORDER BY longalias.a",
        "SELECT DISTINCT a FROM t AS x",
    ] {
        check(d, q);
    }
}

#[test]
fn count_covering_index_uses_table_name_even_when_aliased() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // sqlite's bare `count(*)` covering-index plan is the one scan it labels with
    // the table name rather than the alias — graphite matches that exactly.
    let d = "CREATE TABLE t(a, b); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(1,2),(3,4);";
    check(d, "SELECT count(*) FROM t AS x");
    // No alias: table name either way (regression guard for the unaliased path).
    check(d, "SELECT count(*) FROM t");
}

#[test]
fn aliased_rowid_search_uses_alias() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    let d = "CREATE TABLE t(id INTEGER PRIMARY KEY, b); \
             INSERT INTO t VALUES(1,10),(2,20),(3,30);";
    for q in [
        "SELECT * FROM t AS x WHERE x.id = 2",
        "SELECT * FROM t AS x WHERE id IN (1, 3)",
    ] {
        check(d, q);
    }
}

#[test]
fn aliased_joins_name_each_side_by_alias() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    let d = "CREATE TABLE t(a, b); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(1,2),(3,4),(1,8);";
    for q in [
        "SELECT * FROM t x JOIN t y ON x.a = y.a",
        "SELECT * FROM t AS x, t AS y WHERE x.a = y.a",
        // The inner side of a LEFT join carries sqlite's ` LEFT-JOIN` suffix on its
        // SEARCH node — for an index seek and (below) a rowid seek alike.
        "SELECT * FROM t x LEFT JOIN t y ON x.a = y.a",
    ] {
        check(d, q);
    }

    // Rowid-seek inner side, INNER and LEFT (the LEFT one gets the suffix too).
    let r = "CREATE TABLE u(x, y); CREATE TABLE t(id INTEGER PRIMARY KEY, b); \
             INSERT INTO u VALUES(1,9),(3,8); INSERT INTO t VALUES(1,2),(3,4);";
    check(r, "SELECT * FROM u JOIN t ON t.id = u.x");
    check(r, "SELECT * FROM u LEFT JOIN t ON t.id = u.x");
}

#[test]
#[cfg(feature = "fts5")]
fn aliased_virtual_table_uses_alias() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // A virtual-table scan is also named by its alias.
    let d = "CREATE VIRTUAL TABLE ft USING fts5(x);";
    check(d, "SELECT * FROM ft AS f WHERE x MATCH 'a'");
}
