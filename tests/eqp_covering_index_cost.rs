//! SQLite answers a no-`WHERE` query from a *covering* secondary index when that
//! index's estimated row is strictly narrower than the table's, choosing the
//! narrowest such index (ties broken by the most-recently-created one). This
//! ports SQLite's `estimateTableWidth`/`estimateIndexWidth` cost model — including
//! the per-column size estimate (an integer is 1, a `TEXT`/`BLOB` is 5, a sized
//! `VARCHAR(k)` is `k/4+1`) and the `LogEst` comparison. A `count(*)` needs no
//! columns, so every full index covers it; the same cost choice applies. Verified
//! byte-for-byte (plan and rows) against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn plan(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} EXPLAIN QUERY PLAN {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|c: char| " |`*+_-".contains(c)))
        .collect::<Vec<_>>()
        .join("#")
}

fn rows(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn covering_index_cost_plan_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases: &[(&str, &str)] = &[
        // count(*): narrowest covering index, tie → newest.
        (
            "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c,d);CREATE INDEX tb ON t(b);CREATE INDEX tcd ON t(c,d);",
            "SELECT count(*) FROM t",
        ),
        (
            "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c);CREATE INDEX tb ON t(b);CREATE INDEX tc ON t(c);",
            "SELECT count(*) FROM t",
        ),
        (
            "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c);CREATE INDEX tc ON t(c);CREATE INDEX tb ON t(b);",
            "SELECT count(*) FROM t",
        ),
        // A covering index no narrower than the table is NOT used (SCAN table).
        (
            "CREATE TABLE t(a INTEGER PRIMARY KEY,b TEXT);CREATE INDEX tb ON t(b);",
            "SELECT count(*) FROM t",
        ),
        (
            "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c);CREATE INDEX tbc ON t(b,c);",
            "SELECT b,c FROM t",
        ),
        // Type-width sensitivity (TEXT is szEst 5, VARCHAR(k) is k/4+1).
        (
            "CREATE TABLE t(a INTEGER PRIMARY KEY,b VARCHAR(100),c);CREATE INDEX tb ON t(b);",
            "SELECT count(*) FROM t",
        ),
        (
            "CREATE TABLE t(a INTEGER PRIMARY KEY,b TEXT,c);CREATE INDEX tb ON t(b);",
            "SELECT b FROM t",
        ),
        // No INTEGER PRIMARY KEY (implicit rowid adds to the table width).
        (
            "CREATE TABLE t(a,b,c);CREATE INDEX tb ON t(b);",
            "SELECT count(*) FROM t",
        ),
        // Covered projection picks the narrowest covering index.
        (
            "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c);CREATE INDEX tb ON t(b);CREATE INDEX tbc ON t(b,c);",
            "SELECT b FROM t",
        ),
        // count(*) over an aliased table renders the table name, not the alias.
        (
            "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c);CREATE INDEX tb ON t(b);",
            "SELECT count(*) FROM t x",
        ),
    ];
    for (base, q) in cases {
        assert_eq!(plan("sqlite3", base, q), plan(g, base, q), "plan for `{q}`");
    }
}

/// The covering-index choice changes the row *order* (index order) — it must
/// still match SQLite for a query with no `ORDER BY`.
#[test]
fn covering_index_rows_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c);CREATE INDEX tb ON t(b);\
                CREATE INDEX tbc ON t(b,c);INSERT INTO t VALUES(1,30,7),(2,10,9),(3,20,8),(4,10,5);";
    for q in [
        "SELECT b FROM t",
        "SELECT a FROM t",
        "SELECT b,c FROM t",
        "SELECT count(*) FROM t",
    ] {
        assert_eq!(rows("sqlite3", base, q), rows(g, base, q), "rows for `{q}`");
    }
}
