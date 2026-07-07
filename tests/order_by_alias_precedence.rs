//! `ORDER BY <name>` name resolution when a result-column alias collides with a
//! base table column of the same name. SQLite matches an `ORDER BY` identifier
//! against the result-column aliases FIRST — so `SELECT b AS a FROM t ORDER BY a`
//! sorts by the projected `b`, not the table's own column `a`. graphite's VDBE
//! sort-key builder used to prefer the base column whenever the name matched one
//! (`build_sort_keys` had a `!is_base_column` guard), disagreeing with both sqlite
//! and graphite's own tree-walker. Verified byte-for-byte against sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn rows(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn order_by_alias_shadowing_base_column_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // `a` is a base column (with a NOCASE collation, to make an accidental
    // base-column sort visibly different) and also the alias of the projected `b`.
    let d = "CREATE TABLE t(a TEXT COLLATE NOCASE, b TEXT);\
             INSERT INTO t VALUES('B','y'),('a','x'),('C','z'),('A','w');";
    // Same, but the base column has no explicit collation.
    let d2 = "CREATE TABLE t(a TEXT, b TEXT);\
              INSERT INTO t VALUES('B','y'),('a','x'),('C','z'),('A','w');";
    let n = "CREATE TABLE n(x INT, y INT);INSERT INTO n VALUES(3,1),(1,2),(2,3);";
    let cases: &[String] = &[
        // the bug: the alias `a` (= b) wins over the base column `a`
        format!("{d}SELECT b AS a FROM t ORDER BY a;"),
        format!("{d}SELECT b||'' AS a FROM t ORDER BY a;"),
        format!("{d}SELECT b AS a, a AS realA FROM t ORDER BY a;"),
        format!("{d2}SELECT b AS a FROM t ORDER BY a;"),
        format!("{d2}SELECT a AS b, b AS a FROM t ORDER BY a;"),
        format!("{n}SELECT y AS x FROM n ORDER BY x;"),
        // DESC / COLLATE on the aliased term
        format!("{d}SELECT b AS a FROM t ORDER BY a DESC;"),
        format!("{d}SELECT b AS a FROM t ORDER BY a COLLATE NOCASE;"),
        // must NOT regress: a plain column, a non-colliding alias, ORDER BY a real
        // (non-selected) base column, and an aliased expression
        format!("{d}SELECT a FROM t ORDER BY a;"),
        format!("{d}SELECT a, b FROM t ORDER BY a;"),
        format!("{d}SELECT b FROM t ORDER BY a;"),
        format!("{d}SELECT a AS x FROM t ORDER BY x;"),
        format!("{d}SELECT b AS zz FROM t ORDER BY zz;"),
        format!("{d}SELECT upper(a) AS a FROM t ORDER BY a;"),
        format!("{n}SELECT x, y FROM n ORDER BY x;"),
    ];
    for q in cases {
        assert_eq!(rows("sqlite3", q), rows(g, q), "mismatch for `{q}`");
    }
}
