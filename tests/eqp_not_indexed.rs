//! `EXPLAIN QUERY PLAN` for a table carrying the `NOT INDEXED` planner hint, which
//! forbids every *secondary* index on that table. SQLite then plans a plain full
//! `SCAN` — a `WHERE` seek, covering scan, ORDER-BY index walk, and MULTI-INDEX OR
//! all collapse to it, and the ORDER BY / GROUP BY / DISTINCT sorters re-appear — but
//! the rowid / INTEGER PRIMARY KEY seek (the table's own clustered key) survives, and
//! a lone `min`/`max` still reads one end (`SEARCH t`).
//!
//! graphite's executor already honors the hint, so results were always correct; it
//! ignored the hint in the *plan*, so EXPLAIN diverged (`SEARCH …`/`USING INDEX`
//! where sqlite shows `SCAN`). The plan is now in lockstep. Verified vs the sqlite3
//! 3.50.4 CLI.

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
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); \
                      CREATE INDEX tb ON t(b); CREATE INDEX tc ON t(c);";

#[test]
fn not_indexed_plan_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        // Secondary-index seeks collapse to a plain SCAN.
        "SELECT * FROM t NOT INDEXED WHERE b=5",
        "SELECT * FROM t NOT INDEXED WHERE b>5",
        "SELECT * FROM t NOT INDEXED WHERE b=1 OR c=2",
        "SELECT b FROM t NOT INDEXED", // covering scan → plain SCAN
        // The rowid / INTEGER PRIMARY KEY seek survives the hint.
        "SELECT * FROM t NOT INDEXED WHERE a=3",
        "SELECT * FROM t NOT INDEXED WHERE a>3",
        "SELECT * FROM t NOT INDEXED WHERE a IN (1,2)",
        // A lone min/max still reads one end (`SEARCH t`, no index detail).
        "SELECT max(b) FROM t NOT INDEXED",
        "SELECT min(c) FROM t NOT INDEXED",
        // The ORDER BY / GROUP BY / DISTINCT sorters re-appear (no index walk).
        "SELECT * FROM t NOT INDEXED ORDER BY b",
        "SELECT DISTINCT b FROM t NOT INDEXED",
        "SELECT b FROM t NOT INDEXED GROUP BY b",
        "SELECT b FROM t NOT INDEXED GROUP BY b ORDER BY b",
        // DML carries the hint too.
        "UPDATE t NOT INDEXED SET c=1 WHERE b=5",
        "DELETE FROM t NOT INDEXED WHERE b=5",
        "UPDATE t NOT INDEXED SET c=1 WHERE a=5",
        // The hint absent leaves the index plans intact.
        "SELECT * FROM t WHERE b=5",
        "SELECT * FROM t ORDER BY b",
    ] {
        assert_eq!(
            plan("sqlite3", SCHEMA, q),
            plan(g, SCHEMA, q),
            "plan for {q}"
        );
    }
}

#[test]
fn without_rowid_not_indexed_unchanged() {
    // SQLite serves a WITHOUT ROWID table's clustered PK — and even a covering
    // secondary-index seek — under the hint, which graphite's ordinary path already
    // renders; the collapse-to-SCAN handling must not touch it.
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "CREATE TABLE w(k TEXT PRIMARY KEY, v) WITHOUT ROWID; CREATE INDEX wv ON w(v);";
    for q in [
        "SELECT * FROM w NOT INDEXED WHERE k='a'",
        "SELECT * FROM w NOT INDEXED WHERE v=5",
    ] {
        assert_eq!(plan("sqlite3", base, q), plan(g, base, q), "plan for {q}");
    }
}

#[test]
fn not_indexed_rows_unchanged() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = format!(
        "{SCHEMA} INSERT INTO t VALUES(3,30,300),(1,10,100),(2,20,200),(5,50,500),(4,40,400);"
    );
    for q in [
        "SELECT * FROM t NOT INDEXED WHERE b=20",
        "SELECT a FROM t NOT INDEXED ORDER BY b, a",
        "SELECT b, count(*) FROM t NOT INDEXED GROUP BY b ORDER BY b",
        "SELECT DISTINCT c FROM t NOT INDEXED ORDER BY c",
        "SELECT max(b) FROM t NOT INDEXED",
        "SELECT * FROM t NOT INDEXED WHERE a=4",
    ] {
        assert_eq!(rows("sqlite3", &base, q), rows(g, &base, q), "rows for {q}");
    }
}
