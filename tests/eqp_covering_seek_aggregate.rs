//! Track B (EQP): a WHERE-equality / range / IN seek whose chosen index holds
//! every column the query references — *including* through aggregate/function
//! arguments (`SELECT count(*) … WHERE a=?`, `sum(a) … WHERE a=?`) — is labelled
//! `USING COVERING INDEX`, exactly as sqlite does. Previously graphite only
//! recognised covering for *plain-column* projections, so an aggregate that
//! referenced no uncovered column still rendered the weaker `USING INDEX`.
//!
//! The fix routes the seek covering decision through `query_cols_covered` (which
//! recurses into function args) instead of the plain-projection `index_covers_
//! query`; the executor's covering-seek path uses the same predicate, so the
//! index-only read and the EQP label stay in lockstep. Verified byte-exact
//! against sqlite3 3.50.4, and the result rows match the table-fetch path.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn norm(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).trim_end())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn g_eqp(ddl: &str, q: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let rows = c.query(&format!("EXPLAIN QUERY PLAN {q}")).unwrap().rows;
    let mut lines = Vec::new();
    for r in &rows {
        if let Some(graphitesql::Value::Text(s)) = r.last() {
            lines.push(s.clone());
        }
    }
    lines.join(" | ")
}

fn sqlite_eqp(ddl: &str, q: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{ddl} EXPLAIN QUERY PLAN {q};"))
        .output()
        .unwrap();
    norm(&String::from_utf8_lossy(&o.stdout))
}

/// One row per `value|value` line, so result comparison is exact.
fn g_rows(ddl: &str, q: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let r = c.query(q).unwrap();
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    graphitesql::Value::Null => "".to_string(),
                    graphitesql::Value::Integer(i) => i.to_string(),
                    graphitesql::Value::Real(f) => format!("{f}"),
                    graphitesql::Value::Text(s) => s.clone(),
                    graphitesql::Value::Blob(_) => "<blob>".to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sqlite_rows(ddl: &str, q: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{ddl} {q};"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn check(ddl: &str, q: &str) {
    assert_eq!(g_eqp(ddl, q), sqlite_eqp(ddl, q), "EQP diverged for {q}");
    assert_eq!(g_rows(ddl, q), sqlite_rows(ddl, q), "rows diverged for {q}");
}

#[test]
fn aggregate_over_covered_column_is_covering() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // `ia(a)` covers any query that, through both projection and WHERE, references
    // only `a` (and the rowid). An aggregate with no uncovered argument qualifies.
    let d = "CREATE TABLE t(a, b, c); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(1,2,3),(1,8,9),(4,5,6),(1,2,7);";
    for q in [
        "SELECT count(*) FROM t WHERE a = 1",
        "SELECT sum(a) FROM t WHERE a = 1",
        "SELECT count(*) FROM t WHERE a > 1",
        "SELECT count(*) FROM t WHERE a IN (1, 4)",
        "SELECT a, count(*) FROM t WHERE a = 1 GROUP BY a",
        // Plain covered projection still covering (regression guard).
        "SELECT a FROM t WHERE a = 1 ORDER BY a",
    ] {
        check(d, q);
    }
}

#[test]
fn aggregate_over_uncovered_column_stays_plain_index() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // `ib(b)` is the chosen seek index but does NOT hold `c`; an aggregate over `c`
    // is not covered, so the seek stays `USING INDEX` (graphite reads the table).
    let d = "CREATE TABLE t(a, b, c); CREATE INDEX ib ON t(b); \
             INSERT INTO t VALUES(1,2,3),(1,2,9),(4,5,6);";
    // Both graphite and sqlite agree this is a non-covering seek on ib.
    check(d, "SELECT sum(c) FROM t WHERE b = 2");
    check(d, "SELECT count(c) FROM t WHERE b = 2");
}

#[test]
fn covering_seek_with_null_and_composite_index() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // NULL keys present in the index (a covered equality seek must skip them), and
    // a composite-index two-column equality seek that is covering for an aggregate
    // touching only the indexed columns. (`a IS NULL` index seeking is a separate,
    // pre-existing EQP gap — graphite SCANs there — so it is intentionally omitted.)
    let d = "CREATE TABLE t(a, b); CREATE INDEX ia ON t(a); \
             CREATE INDEX iab ON t(a, b); \
             INSERT INTO t VALUES(1,2),(NULL,3),(1,5),(1,2);";
    check(d, "SELECT count(*) FROM t WHERE a = 1");
    check(d, "SELECT max(b) FROM t WHERE a = 1 AND b = 2");
}
