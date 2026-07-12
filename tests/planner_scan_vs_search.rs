//! Differential test for the planner's SCAN-vs-SEARCH cost comparison.
//!
//! With per-value histogram data (`sqlite_stat4`), sqlite compares the cost of a
//! full table SCAN against a non-covering index SEARCH and picks the cheaper. For
//! a *common* equality value (matching a large fraction of the table) the
//! per-matched-row table lookup makes a full scan cheaper, so sqlite renders
//! `SCAN` rather than `SEARCH`. graphite now ports that comparison
//! (`full_scan_beats_seek`), consuming the same `whereEqualScanEst` row estimate.
//!
//! This shells out to a STAT4-enabled `sqlite3` oracle (via the
//! `GRAPHITE_STAT4_ORACLE` env var, falling back to the in-tree build path) and
//! asserts graphite's `EXPLAIN QUERY PLAN` matches it. Skipped when that binary is
//! absent (the ordinary `sqlite3` on `PATH` is usually built without STAT4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::path::PathBuf;
use std::process::Command;

/// Absolute path to a STAT4-enabled `sqlite3` oracle, or `None` if unavailable.
fn oracle() -> Option<String> {
    if let Ok(p) = std::env::var("GRAPHITE_STAT4_ORACLE")
        && Command::new(&p)
            .arg(":memory:")
            .arg("SELECT 1")
            .output()
            .is_ok()
    {
        return Some(p);
    }
    let default = "/tmp/claude-1000/-home-magicaltux-projects-graphitesql/\
faf0a91b-ae7e-4ff2-9c4e-2e8b1eed5c39/scratchpad/sqlite-src/\
sqlite-amalgamation-3500400/sqlite3-oracle";
    if Command::new(default)
        .arg(":memory:")
        .arg("SELECT 1")
        .output()
        .is_ok()
    {
        return Some(default.to_string());
    }
    None
}

/// Confirm the oracle actually has STAT4 enabled; otherwise skip.
fn oracle_has_stat4(orc: &str) -> bool {
    let out = Command::new(orc)
        .arg(":memory:")
        .arg(
            "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3); CREATE INDEX i ON t(a); \
             ANALYZE; SELECT count(*) FROM sqlite_stat4;",
        )
        .output();
    matches!(out, Ok(o) if String::from_utf8_lossy(&o.stdout).trim().parse::<i64>().unwrap_or(0) > 0)
}

fn orc_query(orc: &str, db: &str, sql: &str) -> String {
    let o = Command::new(orc).arg(db).arg(sql).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn tmp(name: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut p: PathBuf = std::env::temp_dir();
    p.push(format!(
        "gsql-scanseek-{}-{}-{}.db",
        std::process::id(),
        name,
        n
    ));
    let s = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&s);
    s
}

/// Run `EXPLAIN QUERY PLAN <query>` on a fresh in-memory graphite DB built from
/// `setup`, returning just the access line (`SCAN …` / `SEARCH …`).
fn graphite_access(setup: &str, query: &str) -> String {
    let mut conn = Connection::open_memory().unwrap();
    for stmt in setup.split(';') {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        conn.execute(s).unwrap();
    }
    let res = conn.query(&format!("EXPLAIN QUERY PLAN {query}")).unwrap();
    res.rows
        .iter()
        .filter_map(|r| match r.last() {
            Some(graphitesql::Value::Text(s)) if s.contains("SCAN") || s.contains("SEARCH") => {
                Some(String::from(s.as_str()))
            }
            _ => None,
        })
        .next()
        .unwrap_or_default()
}

fn oracle_access(orc: &str, setup: &str, query: &str) -> String {
    let odb = tmp("o");
    let script = format!("{setup} ANALYZE;");
    let out = orc_query(orc, &odb, &format!("{script} EXPLAIN QUERY PLAN {query}"));
    let _ = std::fs::remove_file(&odb);
    out.lines()
        .find(|l| l.contains("SCAN") || l.contains("SEARCH"))
        .map(|l| l.trim_start_matches(['`', '-', ' ']).to_string())
        .unwrap_or_default()
}

/// Assert graphite's SCAN-vs-SEARCH access line matches the oracle's, both built
/// from `setup` + `ANALYZE`.
fn check(orc: &str, setup: &str, query: &str) {
    let o = oracle_access(orc, setup, query);
    let g = graphite_access(&format!("{setup} ANALYZE;"), query);
    assert_eq!(o, g, "SCAN/SEARCH mismatch for `{query}`\nsetup: {setup}");
}

/// 1000 rows with `b=0` in the first `cut` of them; a non-covering index on `b`.
fn skew_setup(cut: usize) -> String {
    format!(
        "CREATE TABLE t(a,b); \
         WITH RECURSIVE c(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM c WHERE i<1000) \
         INSERT INTO t(a,b) SELECT i, CASE WHEN i<={cut} THEN 0 ELSE i END FROM c; \
         CREATE INDEX ib ON t(b);"
    )
}

#[test]
fn scan_vs_search_common_value_full_scans() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    // b=0 is common (900/1000) → non-covering seek needs ~900 table lookups, so a
    // full SCAN is cheaper: both engines SCAN. b=950 is rare (~1 row) → SEARCH.
    let s = skew_setup(900);
    check(&orc, &s, "SELECT * FROM t WHERE b=0;");
    check(&orc, &s, "SELECT * FROM t WHERE b=950;");
    check(&orc, &s, "SELECT a FROM t WHERE b=0;");

    // A covering seek (only the indexed column referenced) never flips to SCAN —
    // it needs no table lookups.
    check(&orc, &s, "SELECT b FROM t WHERE b=0;");
}

#[test]
fn scan_vs_search_threshold_sweep() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    // Sweep the common-value frequency across the SCAN/SEARCH decision boundary;
    // graphite must flip exactly where sqlite 3.50.4 flips (near ~half the table).
    for cut in [100, 300, 440, 450, 460, 480, 500, 550, 700, 900, 990] {
        let s = skew_setup(cut);
        check(&orc, &s, "SELECT * FROM t WHERE b=0;");
        check(&orc, &s, "SELECT * FROM t WHERE b=999;");
    }
}

#[test]
fn scan_vs_search_no_stats_unchanged() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    // WITHOUT ANALYZE the estimate has no stat4 backing, so behavior must be the
    // pre-existing SEARCH regardless of skew. Compare both engines' un-analyzed
    // plan directly (no ANALYZE in the setup).
    let setup = skew_setup(900);
    let g = graphite_access(&setup, "SELECT * FROM t WHERE b=0;");
    let odb = tmp("ns");
    let o = orc_query(
        &orc,
        &odb,
        &format!("{setup} EXPLAIN QUERY PLAN SELECT * FROM t WHERE b=0;"),
    );
    let _ = std::fs::remove_file(&odb);
    let o = o
        .lines()
        .find(|l| l.contains("SCAN") || l.contains("SEARCH"))
        .map(|l| l.trim_start_matches(['`', '-', ' ']).to_string())
        .unwrap_or_default();
    assert_eq!(o, g, "un-analyzed plan should match (both SEARCH)");
    assert!(g.contains("SEARCH"), "un-analyzed should SEARCH, got {g}");
}
