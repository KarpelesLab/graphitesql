//! Row order of an equality seek that pins a *proper prefix* of a composite
//! secondary index.
//!
//! `SELECT … FROM t WHERE a = ?` over `CREATE INDEX i ON t(a, b)` matches every
//! row with `a = ?`, and SQLite walks the index — so within the equal-`a` group
//! the rows come back ordered by the trailing index column `b` (then rowid), not
//! in rowid order. graphite's VDBE executes a single-table query as a rowid-order
//! table scan, so it must defer this shape to the index-walking path to reproduce
//! SQLite's order. A single-column index (`i ON t(a)`) or a *fully* pinned
//! composite prefix (`a = ? AND b = ?`) leaves only the implicit trailing rowid,
//! whose order already *is* rowid order, so those keep the plain scan.
//!
//! Verified byte-for-byte (rows and plan) against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, base: &str, sql: &str) -> String {
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(format!("{base}{sql}"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn composite_prefix_seek_order_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Rowids are deliberately out of `b` order within a=5, so index order (by b)
    // differs from rowid order — the two are distinguishable.
    let composite = "CREATE TABLE t(id INTEGER PRIMARY KEY, a, b, w);\
        CREATE INDEX iab ON t(a,b);\
        INSERT INTO t VALUES(1,5,30,'p'),(2,5,10,'q'),(3,5,20,'r'),(4,6,1,'z');";
    let three = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b,c);\
        CREATE INDEX iabc ON t(a,b,c);\
        INSERT INTO t VALUES(1,1,9,1),(2,1,3,2),(3,1,3,1),(4,1,7,5),(5,2,0,0);";
    let single = "CREATE TABLE t(id INTEGER PRIMARY KEY, k, v);\
        CREATE INDEX ik ON t(k);\
        INSERT INTO t VALUES(5,1,'e'),(3,1,'c'),(1,1,'a'),(2,1,'b');";
    let nulls = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);\
        CREATE INDEX iab ON t(a,b);\
        INSERT INTO t VALUES(1,NULL,30),(2,NULL,10),(3,NULL,20),(4,7,1);";

    let cases: &[(&str, &str)] = &[
        // Covering: read straight from the index, in (a,b) order.
        (composite, "SELECT b FROM t WHERE a=5"),
        // Non-covering: still index order, then fetch each row by rowid.
        (composite, "SELECT b,w FROM t WHERE a=5"),
        (composite, "SELECT id FROM t WHERE a=5"),
        // A trailing ORDER BY makes the order access-path-independent (sanity).
        (composite, "SELECT b FROM t WHERE a=5 ORDER BY b DESC"),
        // Fully-pinned prefix → only the trailing rowid left → rowid order.
        (composite, "SELECT w FROM t WHERE a=5 AND b=10"),
        // Three-column index, one- and two-column equality prefixes.
        (three, "SELECT b,c FROM t WHERE a=1"),
        (three, "SELECT c FROM t WHERE a=1 AND b=3"),
        // Single-column index equality keeps rowid order.
        (single, "SELECT id FROM t WHERE k=1"),
        (single, "SELECT v FROM t WHERE k=1"),
        // A NULL-keyed proper prefix walks the index too.
        (nulls, "SELECT b FROM t WHERE a IS NULL"),
    ];
    for (base, q) in cases {
        assert_eq!(
            run("sqlite3", base, q),
            run(g, base, q),
            "rows diverged for `{q}`"
        );
        let ep = format!("EXPLAIN QUERY PLAN {q}");
        assert_eq!(
            run("sqlite3", base, &ep),
            run(g, base, &ep),
            "plan diverged for `{q}`"
        );
    }
}
