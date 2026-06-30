//! Track B (EQP): SQLite's min/max optimization. A query whose only aggregate is
//! a single `min(col)` / `max(col)` — no `GROUP BY`, `HAVING`, `WHERE`,
//! `DISTINCT`, second aggregate, or other referenced column — reads one end of an
//! ordered scan, so `EXPLAIN QUERY PLAN` renders the access as `SEARCH` rather
//! than `SCAN`. With a covering index on the aggregated column it reads
//! `SEARCH t USING COVERING INDEX <name>`; over an unindexed column it still reads
//! a bare `SEARCH t`.
//!
//! graphite previously labelled both as `SCAN` (it executes the aggregate over an
//! ordinary covering scan — the result is one row, so only the access label
//! differed; the value already matched). `eqp_select` now recognises the shape via
//! `minmax_search_detail` and emits `SEARCH`. The call may be wrapped in scalar
//! expressions (`abs(min(a))`, `max(a)+1`) and may be `min(DISTINCT a)`. Verified
//! byte-exact against sqlite3 3.50.4, both the plan and the result value.
//!
//! Additional referenced columns and expression/constant arguments are handled
//! too. A bare column beside the aggregate (`min(a), b`) makes sqlite seek the one
//! end of an index *leading* with the aggregated column and fetch the other column
//! from the table by rowid (`SEARCH t USING INDEX ia`, non-covering); an expression
//! or constant argument (`min(a+1)`, `min(1)`) keeps only the covering / bare forms.
//! A `WITHOUT ROWID` table is its own clustered primary-key b-tree, so any
//! non-covering seek reads `SEARCH t USING PRIMARY KEY`.
//!
//! Deliberately left to the ordinary access path (rendered differently by sqlite,
//! deferred as out of scope): a `WHERE` clause (sqlite serves the seek from the
//! WHERE index); a second aggregate; a `HAVING`; and the `min(DISTINCT x)` shapes
//! that sqlite renders with a `USE TEMP B-TREE FOR min(DISTINCT)` node — every
//! `DISTINCT` case *except* a lone covered `min(DISTINCT col)`, whose node sqlite
//! elides.

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
fn single_minmax_reads_search() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    let d = "CREATE TABLE t(a, b); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(5,1),(2,2),(8,3),(2,4),(7,5),(NULL,9);";
    for q in [
        // Covering index on the aggregated column -> SEARCH USING COVERING INDEX.
        "SELECT min(a) FROM t",
        "SELECT max(a) FROM t",
        // A scalar wrapper around the call still optimizes.
        "SELECT max(a)+1 FROM t",
        "SELECT abs(min(a)) FROM t",
        // DISTINCT *inside* the aggregate is irrelevant to min/max.
        "SELECT min(DISTINCT a) FROM t",
        // An output alias does not change the access.
        "SELECT min(a) AS m FROM t",
        // A LIMIT leaves the single-row seek intact.
        "SELECT min(a) FROM t LIMIT 1",
        // Unindexed aggregated column: still a bare SEARCH t.
        "SELECT min(b) FROM t",
        "SELECT max(b) FROM t",
    ] {
        check(d, q);
    }
}

#[test]
fn minmax_over_composite_index() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // A composite index covers a single min/max over either of its columns.
    let d = "CREATE TABLE t(a, b, c); CREATE INDEX iab ON t(a, b); \
             INSERT INTO t VALUES(5,1,1),(2,2,2),(8,3,3);";
    for q in [
        "SELECT min(a) FROM t",
        "SELECT min(b) FROM t",
        // `c` is not in the index -> bare SEARCH t.
        "SELECT max(c) FROM t",
    ] {
        check(d, q);
    }
}

#[test]
fn non_minmax_aggregates_stay_scan() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // Regression guards: two aggregates, a non-min/max aggregate, count, GROUP BY,
    // and a plain projection must all keep their existing `SCAN` plans. (No NULL
    // row here: the `GROUP BY b` group would yield a NULL `min(a)` whose blank
    // trailing line the row harness and sqlite's CLI render inconsistently.)
    let d = "CREATE TABLE t(a, b); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(5,1),(2,2),(8,3),(2,4),(7,5);";
    for q in [
        "SELECT min(a), max(a) FROM t",
        "SELECT min(a)+max(a) FROM t",
        "SELECT count(a) FROM t",
        "SELECT sum(b) FROM t",
        "SELECT min(a) FROM t HAVING min(a) > 0",
        "SELECT min(a) FROM t GROUP BY b",
        "SELECT a FROM t",
    ] {
        check(d, q);
    }
}

#[test]
fn minmax_with_other_columns_seeks_non_covering() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // A bare column beside the aggregate is *not* covered by the index on the
    // aggregated column, so sqlite seeks one end of that index and reads the other
    // column from the table (`SEARCH t USING INDEX ia`, no COVERING).
    let d = "CREATE TABLE t(a, b, c); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(5,1,1),(2,2,2),(8,3,3),(2,4,4),(NULL,9,9);";
    for q in [
        "SELECT min(a), b FROM t",
        "SELECT max(a), c FROM t",
        "SELECT min(a), b, c FROM t",
        "SELECT min(a), abs(b) FROM t",
        "SELECT max(a) AS m, b AS x FROM t",
    ] {
        check(d, q);
    }
    // A composite index that covers the extra column keeps the COVERING form; one
    // that does not stays non-covering.
    let dc = "CREATE TABLE t(a, b, c); CREATE INDEX iab ON t(a, b); \
              INSERT INTO t VALUES(5,1,1),(2,2,2),(8,3,3);";
    for q in ["SELECT min(a), b FROM t", "SELECT min(a), c FROM t"] {
        check(dc, q);
    }
    // No index on the aggregated column → bare `SEARCH t` even beside another column.
    let du = "CREATE TABLE t(a, b); INSERT INTO t VALUES(5,1),(2,2),(8,3);";
    check(du, "SELECT min(a), b FROM t");
}

#[test]
fn minmax_over_expression_or_constant_argument() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // `min(<expr>)` still seeks one end (so `SEARCH`), but with no bare column it
    // can only use a *covering* full index or read a bare scan — never the
    // non-covering one-end seek.
    let d = "CREATE TABLE t(a, b); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(5,1),(2,2),(8,3),(2,4);";
    for q in [
        "SELECT min(a+1) FROM t",    // covering ia (only column a referenced)
        "SELECT max(a*2) FROM t",    // covering ia
        "SELECT min(a+1), b FROM t", // b uncovered, expr arg → bare SEARCH t
        "SELECT min(b+1) FROM t",    // b unindexed → bare SEARCH t
        "SELECT min(1) FROM t",      // constant arg → covering ia (nothing to cover)
    ] {
        check(d, q);
    }
}

#[test]
fn minmax_distinct_only_optimizes_the_elided_shape() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // sqlite elides its `USE TEMP B-TREE FOR min(DISTINCT)` node only when the call
    // is the sole result column and a covering index delivers the values — those
    // stay byte-exact as `SEARCH … USING COVERING INDEX`.
    let d = "CREATE TABLE t(a, b); CREATE INDEX ia ON t(a); \
             INSERT INTO t VALUES(5,1),(2,2),(8,3),(2,4);";
    for q in [
        "SELECT min(DISTINCT a) FROM t",
        "SELECT max(DISTINCT a) FROM t",
        "SELECT abs(min(DISTINCT a)) FROM t",
    ] {
        check(d, q);
    }
    let dc = "CREATE TABLE t(a, b, c); CREATE INDEX iab ON t(a, b); \
              INSERT INTO t VALUES(5,1,1),(2,2,2);";
    check(dc, "SELECT min(DISTINCT a) FROM t");

    // The elision needs the DISTINCT column to *lead* the seek b-tree. When it does
    // not — a non-leading index column (`min(DISTINCT b)` over `(a, b)`), an extra
    // reference (`min(DISTINCT a), a`), or no index at all — sqlite keeps the
    // `USE TEMP B-TREE FOR min(DISTINCT)` node, which graphite cannot render. graphite
    // declines those to the ordinary `SCAN t` rather than wrongly claiming a covering
    // seek; this guards against the over-broad elision.
    for q in [
        "SELECT min(DISTINCT b) FROM t", // b is not the leading column of iab
        "SELECT min(DISTINCT a), a FROM t", // a second reference to a
    ] {
        // The min/max optimization declines (no `SEARCH`); graphite falls to its
        // ordinary access path, never a false one-end covering seek.
        let plan = g_eqp(dc, q);
        assert!(
            plan.starts_with("SCAN"),
            "expected SCAN decline for {q}, got {plan}"
        );
        assert_eq!(g_rows(dc, q), sqlite_rows(dc, q), "rows diverged for {q}");
    }
    let du = "CREATE TABLE t(a, b); INSERT INTO t VALUES(5,1),(2,2);";
    assert_eq!(g_eqp(du, "SELECT min(DISTINCT a) FROM t"), "SCAN t");
}

#[test]
fn minmax_over_without_rowid_reads_primary_key() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // A `WITHOUT ROWID` table is its own clustered primary-key b-tree, so a single
    // min/max seek reads `SEARCH t USING PRIMARY KEY` — regardless of which column
    // is aggregated, since the whole row lives in the PK index.
    let d = "CREATE TABLE t(a PRIMARY KEY, b) WITHOUT ROWID; \
             INSERT INTO t VALUES(5,1),(2,2),(8,3);";
    for q in [
        "SELECT min(a) FROM t",
        "SELECT max(a) FROM t",
        "SELECT min(b) FROM t",
        "SELECT abs(min(a)) FROM t",
        "SELECT min(DISTINCT a) FROM t",
    ] {
        check(d, q);
    }
    // A composite primary key behaves the same: still `USING PRIMARY KEY`.
    let dc = "CREATE TABLE t(a, b, c, PRIMARY KEY(a, b)) WITHOUT ROWID; \
              INSERT INTO t VALUES(5,1,1),(2,2,2),(8,3,3);";
    for q in ["SELECT min(a) FROM t", "SELECT min(c) FROM t"] {
        check(dc, q);
    }
    // A secondary index that *covers* the aggregated column is preferred over the
    // clustered PK b-tree (`SEARCH t USING COVERING INDEX ib`); a min over the PK
    // column, which `ib` does not cover, still reads `USING PRIMARY KEY`.
    let di = "CREATE TABLE t(a PRIMARY KEY, b) WITHOUT ROWID; \
              CREATE INDEX ib ON t(b); INSERT INTO t VALUES(5,1),(2,2),(8,3);";
    for q in ["SELECT min(a) FROM t", "SELECT min(b) FROM t"] {
        check(di, q);
    }
    // A bare column beside the aggregate never falls to a non-covering secondary
    // index here — the clustered PK b-tree already carries every column, so the
    // seek stays `USING PRIMARY KEY`.
    let do_ = "CREATE TABLE t(a PRIMARY KEY, b, c) WITHOUT ROWID; CREATE INDEX ia ON t(a); \
               INSERT INTO t VALUES(5,1,1),(2,2,2),(8,3,3);";
    for q in ["SELECT min(a), b FROM t", "SELECT min(b), c FROM t"] {
        check(do_, q);
    }
    // `min(DISTINCT <leading PK column>)` elides the temp-b-tree (the clustered PK
    // already yields the column sorted) → `SEARCH t USING PRIMARY KEY`; a DISTINCT
    // over a *non-leading* column keeps sqlite's `USE TEMP B-TREE FOR min(DISTINCT)`
    // node, which graphite declines to the ordinary `SCAN t`.
    let dd = "CREATE TABLE t(a, b, PRIMARY KEY(a, b)) WITHOUT ROWID; \
              INSERT INTO t VALUES(5,1),(2,2),(8,3);";
    check(dd, "SELECT min(DISTINCT a) FROM t");
    let plan = g_eqp(dd, "SELECT min(DISTINCT b) FROM t");
    assert!(
        plan.starts_with("SCAN"),
        "expected SCAN decline, got {plan}"
    );
    assert_eq!(
        g_rows(dd, "SELECT min(DISTINCT b) FROM t"),
        sqlite_rows(dd, "SELECT min(DISTINCT b) FROM t")
    );
}

#[test]
fn minmax_rowid_aggregate_reads_search() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    // `min(id)`/`max(id)` over the INTEGER PRIMARY KEY reads a bare `SEARCH t`
    // (the rowid b-tree, no named index).
    let d = "CREATE TABLE t(id INTEGER PRIMARY KEY, a); \
             INSERT INTO t VALUES(5,1),(2,2),(8,3);";
    for q in ["SELECT min(id) FROM t", "SELECT max(id) FROM t"] {
        check(d, q);
    }
}
