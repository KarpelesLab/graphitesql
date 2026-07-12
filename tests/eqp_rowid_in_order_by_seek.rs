//! Track B (EQP + execution): a rowid/INTEGER-PRIMARY-KEY `IN`-list — or the
//! same-column equality `OR`-chain that `find_in_constraint` collapses to the same
//! shape — combined with `ORDER BY` on that rowid column elides the
//! `USE TEMP B-TREE FOR ORDER BY`, exactly as sqlite does. sqlite seeks the listed
//! rowids in *sorted* order, so the seek itself already satisfies the ordering: an
//! `ORDER BY id` (ASC) needs no sort, and `ORDER BY id DESC` is served by reversing
//! the ascending seek. graphite now mirrors this by sorting the seek rowids
//! (`in_seek_order`) so the executor emits ascending rowid order, and `run_core`
//! reverses for DESC — a single shared predicate keeps the EQP elision and the
//! executor ordering in lockstep. Verified byte-exact (plan *and* the exact row
//! order, not a sorted multiset — the order is the whole point) against sqlite3
//! 3.50.4.
//!
//! Deferred (still `USE TEMP B-TREE FOR ORDER BY` in graphite, documented in
//! ROADMAP): the same elision for a *secondary*-index `IN` (`a IN(..) ORDER BY a`),
//! for a `WITHOUT ROWID` PK (`k IN(..) ORDER BY k`), and for a bare-`rowid` alias
//! table with no INTEGER PRIMARY KEY.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn g_eqp(c: &Connection, q: &str) -> String {
    c.query(&format!("EXPLAIN QUERY PLAN {q}"))
        .unwrap()
        .rows
        .iter()
        .filter_map(|r| match r.last() {
            Some(Value::Text(s)) => Some(String::from(s.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Row values in the order the engine emits them — order-*dependent* on purpose,
/// since pinning the row order is the point of the ORDER BY elision.
fn g_rows(c: &Connection, q: &str) -> Vec<String> {
    c.query(q)
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|val| match val {
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => format!("{f}"),
                    Value::Text(s) => String::from(s.as_str()),
                    Value::Null => String::new(),
                    _ => "?".into(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn sqlite_out(sql: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn sqlite_rows(sql: &str) -> Vec<String> {
    sqlite_out(sql)
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn sqlite_eqp(ddl: &str, q: &str) -> String {
    sqlite_out(&format!("{ddl} EXPLAIN QUERY PLAN {q};"))
        .lines()
        .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn conn(ddl: &str) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

/// Assert byte-exact EQP *and* exact (ordered) row sequence vs sqlite.
fn check(full: &str, q: &str) {
    let c = conn(full);
    assert_eq!(g_eqp(&c, q), sqlite_eqp(full, q), "EQP diverged for {q}");
    assert_eq!(
        g_rows(&c, q),
        sqlite_rows(&format!("{full} {q};")),
        "row order diverged for {q}"
    );
}

#[test]
fn rowid_in_order_by_elides_temp_btree() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    let full = "CREATE TABLE t(id INTEGER PRIMARY KEY, b); \
                INSERT INTO t VALUES(1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e');";

    // An IN-list or same-column OR-chain on the rowid, ORDER BY that rowid (ASC or
    // DESC), the `rowid` alias, or a multi-term ORDER BY whose lead is the rowid:
    // the per-rowid seek already runs in rowid order, so no temp b-tree is built.
    for q in [
        "SELECT id FROM t WHERE id IN (5,1,3) ORDER BY id",
        "SELECT id FROM t WHERE id IN (5,1,3) ORDER BY id ASC",
        "SELECT id FROM t WHERE id IN (5,1,3) ORDER BY id DESC",
        "SELECT id FROM t WHERE id=5 OR id=1 OR id=3 ORDER BY id",
        "SELECT id FROM t WHERE id=5 OR id=1 OR id=3 ORDER BY id DESC",
        "SELECT id FROM t WHERE id IN (5,1,3) ORDER BY rowid",
        "SELECT id, b FROM t WHERE id IN (5,1,3) ORDER BY id, b",
    ] {
        let c = conn(full);
        let g = g_eqp(&c, q);
        assert!(
            !g.contains("TEMP B-TREE"),
            "rowid IN + ORDER BY should elide the temp b-tree for {q}\n  got: {g}"
        );
        check(full, q);
    }
}

#[test]
fn secondary_index_in_order_by_is_deferred() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // Deferred case (documented in ROADMAP): a *secondary* index IN + ORDER BY on that
    // column. sqlite seeks the index in value order and elides the sort; graphite
    // seeks in list order and still builds the temp b-tree. The rows come out in the
    // same (correct, sorted) order either way — only the plan differs. This test is a
    // tripwire: the row assertion guards correctness now, and the EQP assertion will
    // fail (prompting an update) when the secondary-index elision is implemented.
    let full = "CREATE TABLE t(a, b); CREATE INDEX ia ON t(a); \
                INSERT INTO t VALUES(5,'x'),(1,'y'),(3,'z'),(5,'w');";
    for q in [
        "SELECT a FROM t WHERE a IN (5,1) ORDER BY a",
        "SELECT a FROM t WHERE a=5 OR a=1 ORDER BY a",
    ] {
        let c = conn(full);
        // Rows are correct and correctly ordered in both engines.
        assert_eq!(
            g_rows(&c, q),
            sqlite_rows(&format!("{full} {q};")),
            "row order diverged for {q}"
        );
        // graphite still sorts; sqlite elides via the ordered covering index.
        assert!(
            g_eqp(&c, q).contains("TEMP B-TREE"),
            "expected graphite to still build the temp b-tree (deferred) for {q}"
        );
        assert!(
            !sqlite_eqp(full, q).contains("TEMP B-TREE"),
            "sqlite is expected to elide the temp b-tree for {q}"
        );
    }
}
