//! B9g — an equality prefix on a secondary index followed by a range on the table's
//! rowid seeks the index's implicit trailing key: `WHERE b=? AND rowid>?` reads
//! `SEARCH t USING INDEX ib (b=? AND rowid>?)`, because every secondary-index entry
//! is keyed `(cols…, rowid)`. graphite used to render (and seek) only `(b=?)` and
//! re-filter the rowid bound; it now bounds the `(b, rowid)` range directly, matching
//! SQLite. The rowid range is expressed via the INTEGER PRIMARY KEY column (`a`); the
//! bare `rowid`-alias spelling still renders `(b=?)` in EQP (rows stay correct via the
//! WHERE re-apply) — a small follow-up. Verified vs the sqlite3 3.50.4 CLI.

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

// A single-column index so the rowid tail is the only refinement (a composite index
// would engage cost-based index choice, which is separate — see roadmap B9h).
const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); CREATE INDEX ib ON t(b);";

#[test]
fn trailing_rowid_range_plan_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM t WHERE b=1 AND a>0",
        "SELECT * FROM t WHERE b=1 AND a>0 AND a<9",
        "SELECT * FROM t WHERE b=1 AND a<9",
        "SELECT * FROM t WHERE b=1 AND a>=3",
        "SELECT a FROM t WHERE b=1 AND a>0", // covering
        // No rowid range → plain prefix seek, unchanged.
        "SELECT * FROM t WHERE b=1",
    ] {
        assert_eq!(
            plan("sqlite3", SCHEMA, q),
            plan(g, SCHEMA, q),
            "plan for {q}"
        );
    }
}

#[test]
fn trailing_rowid_range_rows_match() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = format!(
        "{SCHEMA} INSERT INTO t VALUES(1,5,10),(2,5,20),(3,5,30),(4,7,40),(5,5,50),(6,5,60);"
    );
    for q in [
        "SELECT a FROM t WHERE b=5 AND a>2 ORDER BY a",
        "SELECT a FROM t WHERE b=5 AND a>1 AND a<5 ORDER BY a",
        "SELECT a FROM t WHERE b=5 AND a<=3 ORDER BY a",
        "SELECT a FROM t WHERE b=5 AND rowid>=4 ORDER BY a",
        "SELECT count(*) FROM t WHERE b=5 AND a>2",
        "SELECT a FROM t WHERE b=5 AND a>100 ORDER BY a", // empty range
    ] {
        assert_eq!(rows("sqlite3", &base, q), rows(g, &base, q), "rows for {q}");
    }
}
