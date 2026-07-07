//! A correlated reference to an outer column carries that column's affinity when
//! it appears in a comparison inside a subquery. graphite treated the resolved
//! outer value as a bare literal (no affinity), so `text_col = outer_untyped_col`
//! wrongly applied TEXT affinity to the outer value (coercing an integer to text
//! and matching `'2'`), where sqlite compares two columns under BLOB affinity (no
//! coercion) and finds no match — silently returning wrong result rows from an
//! `EXISTS` / `IN (SELECT …)` / scalar correlated subquery. Verified byte-for-byte
//! against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn rows(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut v: Vec<String> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(String::from)
        .collect();
    v.sort();
    v.join("\n")
}

#[test]
fn correlated_comparison_affinity_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases: &[&str] = &[
        // the bug: EXISTS with untyped-outer = TEXT-inner → no coercion, no match
        "CREATE TABLE t(a);CREATE TABLE u(a TEXT);\
         INSERT INTO t VALUES('x'),(2);INSERT INTO u VALUES('2'),('3');\
         SELECT a FROM t WHERE EXISTS(SELECT 1 FROM u WHERE u.a=t.a)",
        // the reverse direction (INTEGER outer, untyped inner) — a match sqlite keeps
        "CREATE TABLE t(a INTEGER);CREATE TABLE u(a);\
         INSERT INTO t VALUES(2),(3);INSERT INTO u VALUES('2');\
         SELECT a FROM t WHERE EXISTS(SELECT 1 FROM u WHERE u.a=t.a)",
        // IN (SELECT …) correlated candidate affinity
        "CREATE TABLE t(a);CREATE TABLE u(a TEXT);\
         INSERT INTO t VALUES('x'),(2);INSERT INTO u VALUES('2');\
         SELECT a FROM t WHERE a IN (SELECT a FROM u)",
        // scalar correlated subquery
        "CREATE TABLE t(a);CREATE TABLE u(a TEXT,v);\
         INSERT INTO t VALUES(2),('3');INSERT INTO u VALUES('2','p');\
         SELECT a,(SELECT v FROM u WHERE u.a=t.a) FROM t ORDER BY a",
        // with an index on the inner column (the seek path)
        "CREATE TABLE t(a);CREATE TABLE u(a TEXT);CREATE INDEX iu ON u(a);\
         INSERT INTO t VALUES('x'),(2),(3);INSERT INTO u VALUES('2'),('3');\
         SELECT a FROM t WHERE EXISTS(SELECT 1 FROM u WHERE u.a=t.a)",
        // --- must NOT regress: same-affinity correlated comparisons ---
        "CREATE TABLE t(a INTEGER);CREATE TABLE u(a INTEGER);\
         INSERT INTO t VALUES(1),(2),(3);INSERT INTO u VALUES(2),(3);\
         SELECT a FROM t WHERE EXISTS(SELECT 1 FROM u WHERE u.a=t.a)",
        "CREATE TABLE t(a TEXT);CREATE TABLE u(a TEXT);\
         INSERT INTO t VALUES('x'),('y');INSERT INTO u VALUES('x');\
         SELECT a FROM t WHERE a IN (SELECT a FROM u)",
        // a correlated comparison against a literal in the inner query is unchanged
        "CREATE TABLE t(a INTEGER);INSERT INTO t VALUES(1),(2);\
         SELECT a FROM t WHERE a=(SELECT 2)",
    ];
    for q in cases {
        let sql = format!("{q};");
        assert_eq!(rows("sqlite3", &sql), rows(g, &sql), "mismatch for `{q}`");
    }
}
