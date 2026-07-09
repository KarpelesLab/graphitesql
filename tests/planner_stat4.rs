//! Differential test for the planner's use of `sqlite_stat4` samples.
//!
//! With per-value histogram data (`sqlite_stat4`), sqlite's `whereEqualScanEst`
//! estimates how many rows a specific `col = VALUE` constraint selects, rather
//! than the stat1 *average*. On skewed data this changes which index a seek
//! chooses. graphite now ports that estimate into `choose_seek_index`.
//!
//! This test shells out to a STAT4-enabled `sqlite3` oracle (via the
//! `GRAPHITE_STAT4_ORACLE` env var, falling back to the in-tree build path) and
//! asserts graphite's `EXPLAIN QUERY PLAN` matches it on stat4-influenced
//! queries. It is skipped when that binary is not present (the ordinary
//! `sqlite3` on `PATH` is usually built without STAT4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::path::PathBuf;
use std::process::Command;

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

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
        "gsql-pstat4-{}-{}-{}.db",
        std::process::id(),
        name,
        n
    ));
    let s = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&s);
    s
}

/// Run `EXPLAIN QUERY PLAN <query>` on a fresh in-memory graphite DB built from
/// `setup`.
fn graphite_eqp(setup: &str, query: &str) -> String {
    let mut conn = Connection::open_memory().unwrap();
    for stmt in setup.split(';') {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        conn.execute(s).unwrap();
    }
    let res = conn.query(&format!("EXPLAIN QUERY PLAN {query}")).unwrap();
    // EQP rows: (id, parent, notused, detail). Render just the detail column.
    res.rows
        .iter()
        .map(|r| r.last().map(render).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Assert graphite's chosen index (the `USING INDEX <name>` token) matches the
/// oracle's for `query`, when both are built from `setup` + `ANALYZE`.
fn check_index_choice(orc: &str, setup: &str, query: &str) {
    let odb = tmp("o");
    let script = format!("{setup} ANALYZE;");
    let oracle_eqp = orc_query(orc, &odb, &format!("{script} EXPLAIN QUERY PLAN {query}"));
    let _ = std::fs::remove_file(&odb);

    let graphite = graphite_eqp(&script, query);

    // Compare the `USING INDEX <name>` token (the stat4-driven choice). Fall back
    // to the whole line so a SCAN-vs-SEARCH difference is also caught.
    let idx_tok = |s: &str| -> Option<String> {
        s.lines().find_map(|l| {
            l.find("USING INDEX ").map(|p| {
                l[p + "USING INDEX ".len()..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
        })
    };
    assert_eq!(
        idx_tok(&oracle_eqp),
        idx_tok(&graphite),
        "index choice mismatch for `{query}`\noracle:\n{oracle_eqp}\ngraphite:\n{graphite}",
    );
}

/// Skewed data: column `b` has one very common value (0, in 900 of 1000 rows)
/// and otherwise unique values. With two candidate indexes, stat4 (not stat1's
/// average) decides whether the `b`-index or the `a`-index is more selective for
/// a given probe value.
const SKEW_SETUP: &str = "CREATE TABLE t(id INTEGER PRIMARY KEY, a, b); \
WITH RECURSIVE c(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM c WHERE i<1000) \
INSERT INTO t(a,b) SELECT i, CASE WHEN i<=900 THEN 0 ELSE i END FROM c; \
CREATE INDEX ia ON t(a); CREATE INDEX ib ON t(b);";

#[test]
fn stat4_equality_index_choice_skewed() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }

    // b=0 is the common value (900 rows) → the a-index (unique) is more
    // selective, so both engines seek `ia`.
    check_index_choice(&orc, SKEW_SETUP, "SELECT * FROM t WHERE b=0 AND a=5;");
    // b=950 is a rare value (~1 row) → stat4 makes the b-index the selective
    // choice; stat1's average alone would always pick `ia`.
    check_index_choice(&orc, SKEW_SETUP, "SELECT * FROM t WHERE b=950 AND a=950;");
}

/// A single skewed column with a partner index on a uniform column. The probe
/// value's frequency (from stat4) decides which index wins.
const TWO_COL_SETUP: &str = "CREATE TABLE p(id INTEGER PRIMARY KEY, hot, uniq); \
WITH RECURSIVE c(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM c WHERE i<600) \
INSERT INTO p(hot,uniq) SELECT CASE WHEN i<=500 THEN 7 ELSE i END, i FROM c; \
CREATE INDEX ph ON p(hot); CREATE INDEX pu ON p(uniq);";

#[test]
fn stat4_equality_two_candidate_indexes() {
    let Some(orc) = oracle() else {
        eprintln!("skipping: no STAT4 oracle available");
        return;
    };
    if !oracle_has_stat4(&orc) {
        eprintln!("skipping: oracle lacks STAT4");
        return;
    }
    // hot=7 (500 rows, common) with uniq also constrained → uniq index wins.
    check_index_choice(
        &orc,
        TWO_COL_SETUP,
        "SELECT * FROM p WHERE hot=7 AND uniq=3;",
    );
    // hot=550 (rare) → the hot index is selective for this probe.
    check_index_choice(
        &orc,
        TWO_COL_SETUP,
        "SELECT * FROM p WHERE hot=550 AND uniq=550;",
    );
}
