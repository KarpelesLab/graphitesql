//! Track B (EQP): SQLite's transient b-tree for `DISTINCT` aggregates. Every
//! `f(DISTINCT col)` aggregate other than `min`/`max` collects its distinct values
//! through a private sorter, rendered in `EXPLAIN QUERY PLAN` as
//! `USE TEMP B-TREE FOR <f>(DISTINCT)` — one node per *unique* such aggregate,
//! placed *before* the scan line (when there is no `GROUP BY`), in result-column
//! order. The function-name tag is the lowercased call name (`count`, `sum`, `avg`,
//! `total`, `group_concat`).
//!
//! The node is emitted exactly when the access path is a bare full `SCAN t`: no
//! index then delivers the distinct values pre-ordered, so each distinct aggregate
//! needs its own sort. A `WHERE`/`ORDER BY` that does not engage an index leaves the
//! scan bare and the node stands; an index that covers or seeks the distinct column
//! changes the scan line and elides the node (handled by the ordinary access path).
//!
//! Two coalescing/elision rules match sqlite's `AggInfo`:
//!   * identical aggregate calls are merged — `count(DISTINCT b)+count(DISTINCT b)`
//!     spills through a single b-tree, not two;
//!   * when the bare scan already yields the distinct column in sorted order (the
//!     rowid-aliasing `INTEGER PRIMARY KEY`, or the leading PK column of a
//!     `WITHOUT ROWID` table) and that single distinct aggregate is the *entire*
//!     computation — one unique distinct aggregate, no other aggregate, no bare
//!     column — sqlite consumes the ordered scan directly and emits no node.
//!     A second aggregate (`count(DISTINCT a), sum(b)`), a bare column
//!     (`count(DISTINCT a), a`), or a non-leading distinct column (`count(DISTINCT b)`)
//!     all defeat the elision and the node reappears.
//!
//! Under `GROUP BY`, the distinct values cannot ride the scan order (that order
//! serves the group key), so nothing is elided: each unique distinct aggregate spills
//! through its own transient b-tree, rendered *after* the `USE TEMP B-TREE FOR GROUP BY`
//! node, in result-column order. The same coalescing applies. When the group key is the
//! rowid (an `INTEGER PRIMARY KEY` grouped by itself) sqlite skips the GROUP-BY sorter
//! entirely and emits no node at all.
//!
//! Deliberately left to a separate slice: `min`/`max(DISTINCT)` (the SEARCH path);
//! multi-argument `DISTINCT` (sqlite rejects it at prepare time). Verified byte-exact
//! against sqlite3 3.50.4, both the plan and the result value.

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
                    graphitesql::Value::Real(f) => {
                        // Match sqlite's CLI: an integer-valued float still prints a
                        // trailing `.0` (`total()` -> `5.0`, not `5`).
                        let s = format!("{f}");
                        if s.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                            format!("{s}.0")
                        } else {
                            s
                        }
                    }
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

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Each distinct aggregate over a bare-scanned table emits its own node, before the
/// scan line, tagged with the lowercased function name.
#[test]
fn distinct_aggregate_emits_temp_btree() {
    if !have_sqlite() {
        return;
    }
    let dc = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(1,5,6),(4,5,9);";
    // One node per call, in result-column order, lowercased name.
    assert_eq!(
        g_eqp(dc, "SELECT count(DISTINCT a) FROM t"),
        "USE TEMP B-TREE FOR count(DISTINCT) | SCAN t"
    );
    for q in [
        "SELECT count(DISTINCT a) FROM t",
        "SELECT sum(DISTINCT a) FROM t",
        "SELECT avg(DISTINCT a) FROM t",
        "SELECT total(DISTINCT a) FROM t",
        "SELECT group_concat(DISTINCT a) FROM t",
        "SELECT count(DISTINCT a)+1 FROM t",
        "SELECT count(DISTINCT a), sum(b) FROM t",
        "SELECT sum(DISTINCT a), avg(DISTINCT b) FROM t",
        "SELECT count(DISTINCT a), sum(DISTINCT b) FROM t",
        "SELECT count(DISTINCT a+1) FROM t",
    ] {
        check(dc, q);
    }
}

/// A `WHERE`/`ORDER BY` that does not engage an index leaves the scan bare, so the
/// node still stands.
#[test]
fn bare_scan_keeps_node_under_where_and_order_by() {
    if !have_sqlite() {
        return;
    }
    let dc = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(1,5,6),(4,5,9);";
    check(dc, "SELECT count(DISTINCT a) FROM t WHERE c>0");
    check(dc, "SELECT count(DISTINCT a) FROM t WHERE a>0");
}

/// An index covering or seeking the distinct column delivers the values pre-ordered,
/// so the ordinary access path takes over and no node is emitted.
#[test]
fn covering_index_elides_node() {
    if !have_sqlite() {
        return;
    }
    let di = "CREATE TABLE t(a,b,c); CREATE INDEX ia ON t(a); INSERT INTO t VALUES(1,2,3),(1,5,6),(4,5,9);";
    // a is covered/ordered by ia -> no node.
    let plan = g_eqp(di, "SELECT count(DISTINCT a) FROM t");
    assert!(
        !plan.contains("TEMP B-TREE"),
        "expected no temp-btree node for covered distinct, got {plan}"
    );
    check(di, "SELECT count(DISTINCT a) FROM t");
    // b is not in the index -> bare scan -> node returns.
    check(di, "SELECT count(DISTINCT b) FROM t");
}

/// SQLite coalesces identical aggregate calls: a doubled `count(DISTINCT b)` spills
/// through a single b-tree, while distinct *kinds* keep separate nodes.
#[test]
fn identical_calls_coalesce() {
    if !have_sqlite() {
        return;
    }
    let dc = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(1,5,6),(4,5,9);";
    assert_eq!(
        g_eqp(dc, "SELECT count(DISTINCT b)+count(DISTINCT b) FROM t"),
        "USE TEMP B-TREE FOR count(DISTINCT) | SCAN t"
    );
    for q in [
        "SELECT count(DISTINCT b)+count(DISTINCT b) FROM t",
        "SELECT count(DISTINCT b), count(DISTINCT b) FROM t",
        "SELECT count(DISTINCT b), sum(DISTINCT b) FROM t",
    ] {
        check(dc, q);
    }
}

/// When the bare scan already yields the distinct column ordered — the rowid-aliasing
/// INTEGER PRIMARY KEY of a rowid table, or the leading PK column of a WITHOUT ROWID
/// table — and that single distinct aggregate is the whole computation, sqlite emits
/// no node. A second aggregate, a bare column, or a non-leading distinct column all
/// bring the node back.
#[test]
fn ordered_scan_elides_lone_leading_distinct() {
    if !have_sqlite() {
        return;
    }
    let ipk = "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c); INSERT INTO t VALUES(1,2,3),(4,5,6);";
    // Elided: lone distinct over the rowid-aliasing PK, possibly self-combined.
    for q in [
        "SELECT count(DISTINCT a) FROM t",
        "SELECT count(DISTINCT a)+count(DISTINCT a) FROM t",
        "SELECT count(DISTINCT a)+1 FROM t",
    ] {
        let plan = g_eqp(ipk, q);
        assert!(
            !plan.contains("TEMP B-TREE"),
            "expected elision for {q}, got {plan}"
        );
        check(ipk, q);
    }
    // Defeated: non-leading column, a bare column, a second aggregate, an expression
    // argument — the node reappears.
    for q in [
        "SELECT count(DISTINCT b) FROM t",
        "SELECT count(DISTINCT a), count(DISTINCT b) FROM t",
        "SELECT count(DISTINCT a), a FROM t",
        "SELECT count(DISTINCT a), sum(b) FROM t",
        "SELECT count(DISTINCT a), sum(DISTINCT a) FROM t",
        "SELECT count(DISTINCT a+1) FROM t",
    ] {
        check(ipk, q);
    }
}

/// A WITHOUT ROWID table's clustered primary key orders the scan by its leading PK
/// column, so a lone distinct over that column is elided; a deeper or non-PK column
/// is not.
#[test]
fn without_rowid_leading_pk_elides() {
    if !have_sqlite() {
        return;
    }
    let wor =
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a)) WITHOUT ROWID; INSERT INTO t VALUES(1,2,3),(4,5,6);";
    let wor2 = "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID; INSERT INTO t VALUES(1,2,3),(4,5,6);";
    let plan = g_eqp(wor, "SELECT count(DISTINCT a) FROM t");
    assert!(
        !plan.contains("TEMP B-TREE"),
        "expected elision over leading PK, got {plan}"
    );
    check(wor, "SELECT count(DISTINCT a) FROM t");
    check(wor, "SELECT count(DISTINCT b) FROM t");
    // Composite PK(a,b): a still leads, b does not.
    check(wor2, "SELECT count(DISTINCT a) FROM t");
    check(wor2, "SELECT count(DISTINCT b) FROM t");
}

/// Under `GROUP BY`, each unique distinct aggregate spills through its own b-tree
/// *after* the GROUP-BY sorter node — nothing is elided, since the scan order serves
/// the group key, not the distinct values.
#[test]
fn group_by_places_node_after_group_sorter() {
    if !have_sqlite() {
        return;
    }
    let dc = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3),(1,5,6),(4,5,9);";
    assert_eq!(
        g_eqp(dc, "SELECT a, count(DISTINCT b) FROM t GROUP BY a"),
        "SCAN t | USE TEMP B-TREE FOR GROUP BY | USE TEMP B-TREE FOR count(DISTINCT)"
    );
    for q in [
        "SELECT a, count(DISTINCT b) FROM t GROUP BY a",
        "SELECT a, count(DISTINCT b), sum(DISTINCT c) FROM t GROUP BY a",
        "SELECT a, count(DISTINCT b), count(DISTINCT b) FROM t GROUP BY a",
        "SELECT a, count(DISTINCT b), sum(c) FROM t GROUP BY a",
        "SELECT a, count(DISTINCT b) FROM t GROUP BY a ORDER BY a",
        "SELECT b, count(DISTINCT c) FROM t GROUP BY b",
    ] {
        check(dc, q);
    }
}

/// Grouping a rowid table by its own `INTEGER PRIMARY KEY` lets sqlite skip the
/// GROUP-BY sorter — and with it any distinct-aggregate node.
#[test]
fn group_by_rowid_key_emits_no_node() {
    if !have_sqlite() {
        return;
    }
    let ipk = "CREATE TABLE t(a INTEGER PRIMARY KEY,b,c); INSERT INTO t VALUES(1,2,3),(4,5,6);";
    for q in [
        "SELECT count(DISTINCT a) FROM t GROUP BY a",
        "SELECT count(DISTINCT b) FROM t GROUP BY a",
    ] {
        let plan = g_eqp(ipk, q);
        assert!(
            !plan.contains("TEMP B-TREE"),
            "expected no temp-btree node grouping by rowid, got {plan}"
        );
        check(ipk, q);
    }
}
