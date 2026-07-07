//! `ORDER BY` term collation resolution, across the compound and the
//! simple/`DISTINCT` execution paths.
//!
//! (1) For a compound (`UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`), the overall
//! `ORDER BY` sorts the combined result; a `COLLATE` written on the term must
//! drive that sort. graphite used to apply only the output column's own collation
//! (from the left `SELECT`), silently ignoring an explicit `ORDER BY 1 COLLATE
//! NOCASE` and sorting BINARY — so `'B' UNION 'a' UNION 'C' ORDER BY 1 COLLATE
//! NOCASE` came out `B,C,a` instead of sqlite's `a,B,C`.
//!
//! (2) In the simple/`DISTINCT` path, an `ORDER BY` term that is a bare position
//! (`ORDER BY 1`) or an output alias must take the collation of the *output
//! column* it names — including an explicit `COLLATE` on that column's projection.
//! graphite honoured this for a plain `SELECT` but not once `DISTINCT` was added,
//! so `SELECT DISTINCT a COLLATE NOCASE FROM t ORDER BY 1` sorted BINARY.
//!
//! Both are fixed (`compound_order_limit` / `order_collations`) and verified
//! byte-for-byte against the sqlite3 3.50.4 CLI.

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
fn compound_order_by_collate_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let nocase = "CREATE TABLE t(a TEXT COLLATE NOCASE);\
                  INSERT INTO t VALUES('B'),('a'),('C');";
    let plain = "CREATE TABLE t(a);INSERT INTO t VALUES('B'),('a'),('C');";
    let cases: &[&str] = &[
        // explicit COLLATE on the compound ORDER BY term (the fix)
        "SELECT 'B' UNION SELECT 'a' UNION SELECT 'C' ORDER BY 1 COLLATE NOCASE;",
        "SELECT 'B' UNION ALL SELECT 'a' UNION ALL SELECT 'C' ORDER BY 1 COLLATE NOCASE;",
        "SELECT 'B' AS x UNION SELECT 'a' UNION SELECT 'C' ORDER BY x COLLATE NOCASE;",
        "SELECT 'B' UNION SELECT 'a' UNION SELECT 'C' ORDER BY 1 COLLATE NOCASE DESC;",
        "SELECT 'B',2 UNION SELECT 'a',1 UNION SELECT 'a',3 ORDER BY 1 COLLATE NOCASE, 2 DESC;",
        // must NOT regress: no explicit COLLATE → BINARY for bare literals …
        "SELECT 'B' UNION SELECT 'a' UNION SELECT 'C' ORDER BY 1;",
        "SELECT 3 UNION SELECT 1 UNION SELECT 2 ORDER BY 1;",
        // … but a column-defined collation still flows into the compound ORDER BY
        &format!("{nocase}SELECT a FROM t UNION SELECT 'D' ORDER BY 1;"),
        &format!("{nocase}SELECT a FROM t UNION ALL SELECT 'D' ORDER BY 1;"),
        // single-select explicit COLLATE unaffected
        &format!("{plain}SELECT a FROM t ORDER BY a COLLATE NOCASE;"),
    ];
    for q in cases {
        assert_eq!(rows("sqlite3", q), rows(g, q), "mismatch for `{q}`");
    }
}

/// In the simple / `DISTINCT` / `GROUP BY` path, a positional or aliased `ORDER
/// BY` term inherits the collation of the output column it names (its explicit
/// `COLLATE`, or a bare column's own collation), and an explicit `COLLATE` on the
/// term itself wins. graphite dropped that inherited collation once `DISTINCT`
/// was present.
#[test]
fn order_by_ordinal_alias_collation_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let d = "CREATE TABLE t(a TEXT, b INT);\
             INSERT INTO t VALUES('B',3),('a',1),('C',2),('b',5),('A',4);";
    let nocol = "CREATE TABLE u(a TEXT COLLATE NOCASE);INSERT INTO u VALUES('B'),('a'),('C');";
    let cases: &[String] = &[
        // the DISTINCT bug: ORDER BY position/alias over a COLLATE'd projection
        format!("{d}SELECT DISTINCT a COLLATE NOCASE FROM t ORDER BY 1;"),
        format!("{d}SELECT DISTINCT a COLLATE NOCASE AS x FROM t ORDER BY x;"),
        // the plain (non-DISTINCT) forms — must stay correct
        format!("{d}SELECT a COLLATE NOCASE FROM t ORDER BY 1;"),
        format!("{d}SELECT a COLLATE NOCASE AS x FROM t ORDER BY x;"),
        // an explicit COLLATE on the term wins, with or without DISTINCT
        format!("{d}SELECT DISTINCT a FROM t ORDER BY a COLLATE NOCASE;"),
        format!("{d}SELECT a FROM t ORDER BY a COLLATE NOCASE DESC;"),
        // a bare position over a column with its own table collation
        format!("{nocol}SELECT a FROM u ORDER BY 1;"),
        format!("{nocol}SELECT a FROM u ORDER BY a COLLATE BINARY;"),
        // GROUP BY + positional ORDER BY under NOCASE
        format!("{d}SELECT a FROM t GROUP BY a COLLATE NOCASE ORDER BY 1;"),
        // no COLLATE anywhere → BINARY; an expression term → its own collation
        format!("{d}SELECT a FROM t ORDER BY 1;"),
        format!("{d}SELECT lower(a) AS x FROM t ORDER BY x;"),
        // multi-key: NOCASE first, integer second
        format!("{d}SELECT a COLLATE NOCASE, b FROM t ORDER BY 1, 2 DESC;"),
    ];
    for q in cases {
        assert_eq!(rows("sqlite3", q), rows(g, q), "mismatch for `{q}`");
    }
}
