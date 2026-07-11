//! A `col = <const>` WHERE equality pins that column to a constant, so sqlite drops
//! a *leading* `ORDER BY` term on it and sorts only the remaining terms —
//! `USE TEMP B-TREE FOR LAST [N TERMS OF] ORDER BY` (or none, when every term is
//! constant/ordered). graphite previously sorted the whole `ORDER BY`. Now the
//! `seek_order_prefix` order-credit also counts equality-constant leading terms, so
//! the temp-b-tree node matches sqlite. Verified differentially against the sqlite3
//! CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, x, y, z); \
                     CREATE INDEX tx ON t(x); \
                     INSERT INTO t VALUES(1,5,3,'a'),(2,5,4,'b'),(3,7,1,'c');";

// Same data but NO index on `x` — the equality `WHERE x = <const>` runs as a plain
// SCAN, exercising the scan-path constant credit (`order_const_lead`).
const SETUP_SCAN: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, x, y, z); \
                          INSERT INTO t VALUES(1,5,3,'a'),(2,5,4,'b'),(3,7,1,'c');";

fn sqlite_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite_eqp_setup(setup: &str, sql: &str) -> Vec<String> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{setup} EXPLAIN QUERY PLAN {sql};"))
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.trim_start_matches(['`', '|', '-', ' ']).to_string())
        .filter(|s| !s.is_empty() && s != "QUERY PLAN")
        .collect()
}

fn sqlite_eqp(sql: &str) -> Vec<String> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{SETUP} EXPLAIN QUERY PLAN {sql};"))
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.trim_start_matches(['`', '|', '-', ' ']).to_string())
        .filter(|s| !s.is_empty() && s != "QUERY PLAN")
        .collect()
}

fn graphite_eqp(c: &Connection, sql: &str) -> Vec<String> {
    c.query(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .rows
        .iter()
        .map(|r| match r.last() {
            Some(Value::Text(t)) => t.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn constant_leading_order_by_terms_are_dropped_like_sqlite() {
    if !sqlite_available() {
        return;
    }
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    for q in [
        // `x` constant → dropped; only `y` (and `z`) need the temp b-tree.
        "SELECT * FROM t WHERE x=5 ORDER BY x, y",
        "SELECT * FROM t WHERE x=5 ORDER BY x, y, z",
        // Two constants dropped → only `z` sorted.
        "SELECT * FROM t WHERE x=5 AND y=3 ORDER BY x, y, z",
        // Every term constant/ordered → no temp b-tree at all.
        "SELECT * FROM t WHERE x=5 ORDER BY x",
        "SELECT * FROM t WHERE x=5 AND y=3 ORDER BY x, y",
        // A *trailing* constant after a non-constant leading term does NOT shrink the
        // sort (the leading term must be satisfied first) → whole ORDER BY sorted.
        "SELECT * FROM t WHERE x=5 ORDER BY y, x",
        // Regression guards: a range seek (walk-ordered, not constant), a DESC term,
        // a no-WHERE scan, and an ORDER BY on a non-constant column only.
        "SELECT * FROM t WHERE x>1 ORDER BY x, y",
        "SELECT * FROM t WHERE x=5 ORDER BY x DESC, y",
        "SELECT * FROM t ORDER BY x, y",
        "SELECT * FROM t WHERE x=5 ORDER BY y",
    ] {
        assert_eq!(sqlite_eqp(q), graphite_eqp(&c, q), "EQP diverged on `{q}`");
    }
}

#[test]
fn constant_terms_dropped_on_a_plain_scan_too() {
    if !sqlite_available() {
        return;
    }
    // No index on `x`, so `WHERE x = <const>` runs as a plain SCAN — the constant
    // credit must still drop the leading `x`.
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP_SCAN.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    for q in [
        "SELECT * FROM t WHERE x=5 ORDER BY x, y",
        "SELECT * FROM t WHERE x=5 AND y=3 ORDER BY x, y, z",
        "SELECT * FROM t WHERE x=5 ORDER BY x",
        // A non-constant leading term still sorts the whole clause.
        "SELECT * FROM t WHERE x=5 ORDER BY y, x",
        "SELECT * FROM t WHERE x=5 ORDER BY y",
        "SELECT * FROM t ORDER BY x, y",
    ] {
        assert_eq!(
            sqlite_eqp_setup(SETUP_SCAN, q),
            graphite_eqp(&c, q),
            "EQP diverged on `{q}`"
        );
    }
}

#[test]
fn single_row_driver_join_all_constant_order_by() {
    if !sqlite_available() {
        return;
    }
    // `big.id=7` is a single-row rowid seek, so `big.*` is constant and `small.v`
    // (equated to `big.k` by the ON) is constant too — an ORDER BY on any of those
    // needs no sort. A non-constant inner column (`small.k`) still sorts.
    // `small.v` is numeric so `big.k = small.v` actually joins (big.id=7 → k=2 →
    // matches small rows 1 and 2, whose v=2); `small.k` varies so it is genuinely
    // non-constant.
    const S: &str = "CREATE TABLE big(id INTEGER PRIMARY KEY, k, v); \
                     CREATE TABLE small(id INTEGER PRIMARY KEY, k, v); \
                     INSERT INTO big VALUES(5,1,'b5'),(7,2,'b7'),(9,2,'b9'); \
                     INSERT INTO small VALUES(1,8,2),(2,7,2),(3,6,1);";
    let mut c = Connection::open_memory().unwrap();
    for stmt in S.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    let eqp = |sql: &str| -> Vec<String> {
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg(format!("{S} EXPLAIN QUERY PLAN {sql};"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| l.trim_start_matches(['`', '|', '-', ' ']).to_string())
            .filter(|s| !s.is_empty() && s != "QUERY PLAN")
            .collect()
    };
    for q in [
        // Join-equated inner column → constant → no sort.
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.v",
        // Driver column → constant → no sort.
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY big.v",
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY big.v, small.v",
        // A non-constant inner column still needs the sort.
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.k",
        // The inner's own rowid (ascending) is satisfied by the inner's plain
        // rowid-order scan (small has no secondary index) — no sort.
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.id",
        "SELECT * FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.v, small.id",
    ] {
        assert_eq!(eqp(q), graphite_eqp(&c, q), "EQP diverged on `{q}`");
    }
    // The rows (as a set) are unchanged when the sort is skipped.
    let mut got: Vec<i64> = c
        .query("SELECT small.id FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY big.v")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(n) => *n,
            v => panic!("{v:?}"),
        })
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2]);

    // The inner-rowid ORDER BY skips the sort, but the inner's plain rowid scan
    // already yields ascending small.id — so the rows come out ordered.
    let ordered: Vec<i64> = c
        .query(
            "SELECT small.id FROM big JOIN small ON big.k=small.v WHERE big.id=7 ORDER BY small.id",
        )
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(n) => *n,
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(ordered, vec![1, 2]);
}

#[test]
fn dropped_constant_term_still_returns_correct_rows() {
    if !sqlite_available() {
        return;
    }
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    // `WHERE x=5 ORDER BY x, y` must still come out sorted by `y` (x is the constant 5).
    let ys: Vec<i64> = c
        .query("SELECT y FROM t WHERE x=5 ORDER BY x, y")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(n) => *n,
            v => panic!("{v:?}"),
        })
        .collect();
    assert_eq!(ys, vec![3, 4]);
}
