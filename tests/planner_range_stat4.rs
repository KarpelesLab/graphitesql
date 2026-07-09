//! Differential test for the planner's STAT4-driven RANGE selectivity estimate.
//!
//! With per-value histogram data (`sqlite_stat4`), sqlite's `whereRangeScanEst`
//! estimates how many rows a range predicate (`col > x`, `col < y`,
//! `col BETWEEN x AND y`, …) selects by binary-searching the samples for the
//! bounds, rather than the fixed 1/4-per-inequality heuristic. That estimate then
//! feeds the SCAN-vs-SEARCH cost comparison: an open-ended range matching a large
//! fraction of the table is cheaper to full-SCAN than to seek through a
//! non-covering index. graphite ports both (`stat4::range_scan_est` +
//! `full_scan_beats_range`).
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
        "gsql-range-stat4-{}-{}-{}.db",
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
                Some(s.clone())
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
    assert_eq!(
        o, g,
        "range SCAN/SEARCH mismatch for `{query}`\nsetup: {setup}"
    );
}

/// 1000-row rowid table with a uniformly increasing column `a` and a partner
/// column `b` (skewed: 0 in the first 900 rows). Non-covering secondary indexes
/// on both columns; `pad` keeps every candidate index non-covering for `SELECT *`.
const SETUP: &str = "CREATE TABLE t(id INTEGER PRIMARY KEY, a, b, pad); \
WITH RECURSIVE c(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM c WHERE i<1000) \
INSERT INTO t(a,b,pad) SELECT i, CASE WHEN i<=900 THEN 0 ELSE i END, i FROM c; \
CREATE INDEX ia ON t(a); CREATE INDEX ib ON t(b);";

#[test]
fn range_open_ended_scan_vs_search() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    // `a` is uniform 1..1000. A range matching a large fraction of the table is
    // cheaper to full-SCAN than to seek through the non-covering index `ia`; a
    // selective one keeps the SEARCH. graphite must agree with sqlite 3.50.4 on
    // both sides of the boundary and on every operator.
    check(&orc, SETUP, "SELECT * FROM t WHERE a>100;"); // ~90% → SCAN
    check(&orc, SETUP, "SELECT * FROM t WHERE a>990;"); // ~1% → SEARCH
    check(&orc, SETUP, "SELECT * FROM t WHERE a>=500;");
    check(&orc, SETUP, "SELECT * FROM t WHERE a<200;"); // ~20% → SCAN
    check(&orc, SETUP, "SELECT * FROM t WHERE a<=990;");
    check(&orc, SETUP, "SELECT * FROM t WHERE a<10;"); // ~1% → SEARCH
}

#[test]
fn range_two_sided_scan_vs_search() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    check(&orc, SETUP, "SELECT * FROM t WHERE a>5 AND a<900;"); // wide → SCAN
    check(&orc, SETUP, "SELECT * FROM t WHERE a>5 AND a<10;"); // narrow → SEARCH
    check(&orc, SETUP, "SELECT * FROM t WHERE a BETWEEN 10 AND 800;"); // wide → SCAN
    check(&orc, SETUP, "SELECT * FROM t WHERE a BETWEEN 490 AND 500;"); // narrow → SEARCH
}

#[test]
fn range_skewed_selectivity() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    // `b` is 0 in 900 of 1000 rows, else unique. A stat1 *average* would badly
    // misjudge these; the stat4 samples make `b>0` (all-but-the-hot-value ≈ 100
    // rows, still ~10%) vs `b>950` (a handful) decide correctly.
    check(&orc, SETUP, "SELECT * FROM t WHERE b>0;");
    check(&orc, SETUP, "SELECT * FROM t WHERE b>950;");
    check(&orc, SETUP, "SELECT * FROM t WHERE b BETWEEN 901 AND 999;");
    check(&orc, SETUP, "SELECT * FROM t WHERE b BETWEEN 0 AND 999;");
}

#[test]
fn range_boundary_sweep() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    // Sweep `a > N` across the SCAN/SEARCH decision boundary (sqlite 3.50.4 flips
    // between N=550 and N=560 for this shape); graphite must flip in lockstep.
    for n in [100, 300, 500, 540, 550, 560, 600, 700, 850, 900, 950, 990] {
        check(&orc, SETUP, &format!("SELECT * FROM t WHERE a>{n};"));
    }
}

#[test]
fn range_text_and_real_bounds() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    // A TEXT key: bounds compare with the column's collation; the non-covering
    // seek-vs-scan decision must still track sqlite (the `pad` column keeps the
    // index non-covering for `SELECT *`).
    let text = "CREATE TABLE t(id INTEGER PRIMARY KEY, s TEXT, pad); \
WITH RECURSIVE c(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM c WHERE i<1000) \
INSERT INTO t(s,pad) SELECT printf('%04d', i), i FROM c; CREATE INDEX is1 ON t(s);";
    check(&orc, text, "SELECT * FROM t WHERE s>'0100';"); // wide → SCAN
    check(&orc, text, "SELECT * FROM t WHERE s>'0990';"); // selective → SEARCH
    check(
        &orc,
        text,
        "SELECT * FROM t WHERE s BETWEEN '0100' AND '0800';",
    );

    // A REAL key.
    let real = "CREATE TABLE t(id INTEGER PRIMARY KEY, x REAL, pad); \
WITH RECURSIVE c(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM c WHERE i<1000) \
INSERT INTO t(x,pad) SELECT i*1.5, i FROM c; CREATE INDEX ix ON t(x);";
    check(&orc, real, "SELECT * FROM t WHERE x>150.0;"); // wide → SCAN
    check(&orc, real, "SELECT * FROM t WHERE x>1450.0;"); // selective → SEARCH
}

#[test]
fn range_desc_index() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    // A DESC leading index column reverses value order in key space; the stat4
    // range estimator swaps the bounds to match, so the decision is unchanged.
    let desc = "CREATE TABLE t(id INTEGER PRIMARY KEY, a, pad); \
WITH RECURSIVE c(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM c WHERE i<1000) \
INSERT INTO t(a,pad) SELECT i, i FROM c; CREATE INDEX ia ON t(a DESC);";
    check(&orc, desc, "SELECT * FROM t WHERE a>100;");
    check(&orc, desc, "SELECT * FROM t WHERE a>990;");
    check(&orc, desc, "SELECT * FROM t WHERE a BETWEEN 100 AND 800;");
}

#[test]
fn range_no_stats_unchanged() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    // WITHOUT ANALYZE there is no stat4 backing, so graphite keeps its prior
    // always-SEARCH range behavior — matching an un-analyzed sqlite exactly.
    let g = graphite_access(SETUP, "SELECT * FROM t WHERE a>100;");
    let odb = tmp("ns");
    let o = orc_query(
        &orc,
        &odb,
        &format!("{SETUP} EXPLAIN QUERY PLAN SELECT * FROM t WHERE a>100;"),
    );
    let _ = std::fs::remove_file(&odb);
    let o = o
        .lines()
        .find(|l| l.contains("SCAN") || l.contains("SEARCH"))
        .map(|l| l.trim_start_matches(['`', '-', ' ']).to_string())
        .unwrap_or_default();
    assert_eq!(o, g, "un-analyzed range plan should match (both SEARCH)");
    assert!(
        g.contains("SEARCH"),
        "un-analyzed range should SEARCH, got {g}"
    );
}

#[test]
fn range_indexed_by_hint_forces_seek() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    // An `INDEXED BY` hint forces the seek: sqlite never falls back to a SCAN even
    // when the range is unselective, and neither may graphite.
    check(&orc, SETUP, "SELECT * FROM t INDEXED BY ia WHERE a>100;");
    check(&orc, SETUP, "SELECT * FROM t INDEXED BY ia WHERE a>990;");
}
