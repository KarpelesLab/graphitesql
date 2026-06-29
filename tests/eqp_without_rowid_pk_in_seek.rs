//! Track B (EQP + execution): an `IN`-list — or the equivalent same-column equality
//! `OR`-chain, which `find_in_constraint` collapses to the same shape — on the
//! *leading* PRIMARY KEY column of a `WITHOUT ROWID` table seeks the clustered b-tree
//! once per value (`try_without_rowid_pk_in`) instead of scanning, rendering one
//! `SEARCH … USING PRIMARY KEY (k=?)` exactly as sqlite plans it. Previously graphite
//! recognised only single-equality and range bounds on the WITHOUT ROWID PK, so an
//! `IN`/`OR` fell to a full `SCAN`. Each distinct leading-PK value addresses a disjoint
//! slice of the b-tree, so repeated/duplicate list values are de-duplicated and the
//! result is a valid superset (`run_core` re-applies the full WHERE). A non-leading-PK
//! column, or a column with no usable index, still scans. Verified byte-exact (plan
//! and row set) against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn g_eqp(c: &Connection, q: &str) -> String {
    c.query(&format!("EXPLAIN QUERY PLAN {q}"))
        .unwrap()
        .rows
        .iter()
        .filter_map(|r| match r.last() {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Row values as a sorted multiset — order-independent, since a per-value clustered
/// seek has no inherent row order. The EQP assertions pin the plan; this the contents.
fn g_rows_sorted(c: &Connection, q: &str) -> Vec<String> {
    let mut v = c
        .query(q)
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|val| match val {
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => format!("{f}"),
                    Value::Text(s) => s.clone(),
                    Value::Null => String::new(),
                    _ => "?".into(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>();
    v.sort();
    v
}

fn sqlite_out(sql: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn sqlite_rows_sorted(sql: &str) -> Vec<String> {
    let mut v: Vec<String> = sqlite_out(sql)
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect();
    v.sort();
    v
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

fn check(full: &str, q: &str) {
    let c = conn(full);
    let g = g_eqp(&c, q);
    assert_eq!(g, sqlite_eqp(full, q), "EQP diverged for {q}");
    assert_eq!(
        g_rows_sorted(&c, q),
        sqlite_rows_sorted(&format!("{full} {q};")),
        "rows diverged for {q}"
    );
}

#[test]
fn without_rowid_pk_in_and_or_chain_seek() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    let text = "CREATE TABLE t(k TEXT PRIMARY KEY, v) WITHOUT ROWID; \
                INSERT INTO t VALUES('a',1),('b',2),('c',3),('d',4);";

    // IN-list and same-column equality OR-chain on the PK both collapse to one
    // clustered seek (the OR-chain via find_in_constraint). A NULL list entry never
    // matches but, as in the rowid/secondary IN branch, doesn't change the plan label.
    // A repeated value de-duplicates. A trailing AND only narrows the superset.
    let seek: &[&str] = &[
        "SELECT * FROM t WHERE k IN ('a','c')",
        "SELECT * FROM t WHERE k='a' OR k='c'",
        "SELECT * FROM t WHERE k='a' OR k='c' OR k='d'",
        "SELECT * FROM t WHERE k IN ('a','a','c')",
        "SELECT * FROM t WHERE k IN ('a',NULL)",
        "SELECT * FROM t WHERE (k='a' OR k='c') AND v>1",
        "SELECT * FROM t WHERE 'a'=k OR k='c'",
    ];
    for &q in seek {
        let c = conn(text);
        let g = g_eqp(&c, q);
        assert!(
            !g.contains("MULTI-INDEX OR") && !g.contains("SCAN"),
            "WITHOUT ROWID PK IN/OR should seek for {q}\n  got: {g}"
        );
        assert_eq!(g, "SEARCH t USING PRIMARY KEY (k=?)", "for {q}");
        check(text, q);
    }

    // An INTEGER PRIMARY KEY WITHOUT ROWID is a normal PK b-tree (not a rowid alias).
    let int = "CREATE TABLE t(k INTEGER PRIMARY KEY, v) WITHOUT ROWID; \
               INSERT INTO t VALUES(1,10),(2,20),(3,30),(5,50);";
    for q in [
        "SELECT * FROM t WHERE k IN (1,3,5)",
        "SELECT * FROM t WHERE k=1 OR k=3",
    ] {
        let c = conn(int);
        assert_eq!(g_eqp(&c, q), "SEARCH t USING PRIMARY KEY (k=?)", "for {q}");
        check(int, q);
    }
}

#[test]
fn without_rowid_composite_pk_leading_in_seek() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // Composite PK(a,b): an IN/OR on the *leading* column `a` prefix-seeks the b-tree
    // (each value can match several rows); on the *trailing* column `b` it must scan.
    let full = "CREATE TABLE t(a,b,v,PRIMARY KEY(a,b)) WITHOUT ROWID; \
                INSERT INTO t VALUES(1,1,'x'),(1,2,'y'),(2,1,'z'),(3,9,'w');";

    for q in [
        "SELECT * FROM t WHERE a IN (1,3)",
        "SELECT * FROM t WHERE a=1 OR a=3",
    ] {
        let c = conn(full);
        assert_eq!(g_eqp(&c, q), "SEARCH t USING PRIMARY KEY (a=?)", "for {q}");
        check(full, q);
    }

    // Trailing-column IN is not a usable PK prefix → SCAN in both engines.
    let c = conn(full);
    assert_eq!(g_eqp(&c, "SELECT * FROM t WHERE b IN (1,2)"), "SCAN t");
    check(full, "SELECT * FROM t WHERE b IN (1,2)");
}

#[test]
fn without_rowid_non_pk_in_scans() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // `v` is unindexed: an IN/OR on it scans (no MULTI-INDEX OR, no PK seek).
    let full = "CREATE TABLE t(k TEXT PRIMARY KEY, v) WITHOUT ROWID; \
                INSERT INTO t VALUES('a',1),('b',2),('c',3),('d',4);";
    for q in [
        "SELECT * FROM t WHERE v IN (1,3)",
        "SELECT * FROM t WHERE v=1 OR v=3",
    ] {
        let c = conn(full);
        assert_eq!(g_eqp(&c, q), "SCAN t", "for {q}");
        check(full, q);
    }
}
