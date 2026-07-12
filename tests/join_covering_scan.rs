//! Cost-based covering-index scan for a table SCANNED inside a JOIN: when a plain
//! secondary index holds every column of a scanned table (the outer driver, or a
//! materialised/nested-loop inner of an INNER/CROSS join) that the query
//! references — and that index is strictly narrower than the table (SQLite's
//! `estimateTableWidth`/`estimateIndexWidth` `LogEst` model) — SQLite scans that
//! table via the covering index, visiting its rows in index-key order. That
//! changes the join's output ROW ORDER for an unordered query, not just the plan.
//! Graphite matches: `SCAN <t> USING COVERING INDEX <idx>` in EXPLAIN QUERY PLAN
//! and the same visited order in the rows.
//!
//! Differential against real sqlite3 3.50.4 (skipped if the binary is absent),
//! asserting BOTH the plan and the rows, on `set_use_vdbe(true)` (default) and
//! `(false)` — the VDBE join path defers the covering-order shapes to the
//! tree-walker, which owns the reorder, so both modes match SQLite.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3(script: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(script)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn have_sqlite3() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn fmt(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => {
            if *r == (*r as i64) as f64 {
                format!("{:.1}", r)
            } else {
                format!("{r}")
            }
        }
        Value::Text(s) => String::from(s.as_str()),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

fn graphite_rows(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(fmt).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn graphite_plan(c: &Connection, sql: &str) -> Vec<String> {
    c.query(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .rows
        .iter()
        .map(|row| match row.last() {
            Some(Value::Text(s)) => String::from(s.as_str()),
            other => panic!("plan detail not text: {other:?}"),
        })
        .collect()
}

fn sqlite_plan(setup: &str, sql: &str) -> Vec<String> {
    let out = sqlite3(&format!("{setup}\nEXPLAIN QUERY PLAN {sql};"));
    out.lines()
        .filter(|l| *l != "QUERY PLAN")
        .map(|l| l.trim_start_matches(['|', '-', '`', ' ']).to_string())
        .collect()
}

fn graphite(setup: &str, use_vdbe: bool) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.set_use_vdbe(use_vdbe);
    for stmt in setup.split(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

/// Assert graphite's rows for `sql` equal sqlite's, on BOTH VDBE modes.
fn assert_rows(setup: &str, sql: &str) {
    let want = sqlite3(&format!("{setup}\n{sql};"));
    for &vdbe in &[true, false] {
        let c = graphite(setup, vdbe);
        let got = graphite_rows(&c, sql);
        assert_eq!(got, want, "rows diverged (use_vdbe={vdbe}) for `{sql}`");
    }
}

/// Assert graphite's EXPLAIN QUERY PLAN detail lines equal sqlite's.
fn assert_plan(setup: &str, sql: &str) {
    let want = sqlite_plan(setup, sql);
    let c = graphite(setup, true);
    let got = graphite_plan(&c, sql);
    assert_eq!(got, want, "plan diverged for `{sql}`");
}

/// Assert graphite's plan for `sql` on graphite matches an EXACT expected list.
fn assert_plan_is(setup: &str, sql: &str, want: &[&str]) {
    let c = graphite(setup, true);
    let got = graphite_plan(&c, sql);
    assert_eq!(got, want, "plan mismatch for `{sql}`");
}

// `u` has a rowid IPK `x`; `v` is a wide table (a large TEXT column so a narrow
// index on `p` is strictly narrower than the row), indexed on `p` via `iv`. The
// values are asymmetric so rowid order and `p`-key order are distinguishable.
const SETUP: &str = "CREATE TABLE u(x INTEGER PRIMARY KEY, y);\
                     CREATE TABLE v(p, q TEXT);\
                     CREATE INDEX iv ON v(p);\
                     INSERT INTO u VALUES(3,30),(1,10),(2,20);\
                     INSERT INTO v VALUES(2,'two'),(1,'one'),(3,'three'),(1,'uno');";

#[test]
fn cross_join_covered_inner_bug_case() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // The reported bug: only `v.p` is needed from the inner `v`, and `iv` covers
    // it. sqlite scans `v` via `iv` (p order), so the joined rows come out in that
    // order — plan AND rows must match.
    assert_plan(SETUP, "SELECT u.x, v.p FROM u CROSS JOIN v");
    assert_rows(SETUP, "SELECT u.x, v.p FROM u CROSS JOIN v");
    assert_plan_is(
        SETUP,
        "SELECT u.x, v.p FROM u CROSS JOIN v",
        &["SCAN u", "SCAN v USING COVERING INDEX iv"],
    );
}

#[test]
fn inner_join_covered_inner_via_rowid_swap() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // `u JOIN v ON u.x=v.p`: sqlite drives from `v` (scanned via `iv`, since `u.x`
    // is the rowid IPK and only `v.p` is needed) and seeks `u` by rowid. The
    // covering driver scan fixes the row order.
    assert_plan(SETUP, "SELECT u.x, v.p FROM u JOIN v ON u.x=v.p");
    assert_rows(SETUP, "SELECT u.x, v.p FROM u JOIN v ON u.x=v.p");
}

#[test]
fn covered_driver_with_rowid_seek_inner() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // `v` is the driver (FROM-first) and only `v.p` is needed from it → covering
    // scan of `v` via `iv`; the inner `u` is rowid-sought. Rows follow `v`'s
    // index-key order.
    assert_plan(SETUP, "SELECT v.p, u.y FROM v JOIN u ON u.x=v.p");
    assert_rows(SETUP, "SELECT v.p, u.y FROM v JOIN u ON u.x=v.p");
}

#[test]
fn distinct_over_covered_join() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // DISTINCT over the covered join: the rows (a set) must match regardless of the
    // covering scan; sqlite additionally renders a TEMP B-TREE for DISTINCT.
    assert_rows(SETUP, "SELECT DISTINCT v.p FROM u CROSS JOIN v");
    assert_rows(SETUP, "SELECT DISTINCT u.y FROM u JOIN v ON u.x=v.p");
}

#[test]
fn group_by_over_covered_join() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    assert_rows(
        SETUP,
        "SELECT v.p, count(*) FROM u CROSS JOIN v GROUP BY v.p",
    );
}

#[test]
fn order_by_over_covered_join_is_invariant() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // An explicit ORDER BY makes the row order access-path-independent; both VDBE
    // modes may run it and the rows match sqlite.
    assert_rows(
        SETUP,
        "SELECT u.x, v.p FROM u CROSS JOIN v ORDER BY u.x, v.p",
    );
}

#[test]
fn left_join_covered_inner_rows_and_order() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // A LEFT JOIN whose inner `v` is scanned via a covering index: the null-
    // extension semantics are untouched (which rows are produced never changes),
    // only the inner scan mechanism/order. Rows and plan match sqlite. Use a driver
    // row (x=5) with no match so the LEFT-null-extension is exercised.
    let setup = "CREATE TABLE u(x INTEGER PRIMARY KEY, y);\
                 CREATE TABLE v(p, q TEXT);\
                 CREATE INDEX iv ON v(p);\
                 INSERT INTO u VALUES(3,30),(5,50),(1,10);\
                 INSERT INTO v VALUES(2,'two'),(1,'one'),(3,'three');";
    assert_plan(setup, "SELECT u.x, v.p FROM u LEFT JOIN v ON 1=1");
    assert_rows(setup, "SELECT u.x, v.p FROM u LEFT JOIN v ON 1=1");
}

// ---- Negative / unchanged cases ---------------------------------------------

#[test]
fn no_covering_index_stays_plain_scan() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // No secondary index on `v` at all → plain SCAN, rowid order (unchanged).
    let setup = "CREATE TABLE u(x INTEGER PRIMARY KEY, y);\
                 CREATE TABLE v(p, q);\
                 INSERT INTO u VALUES(3,30),(1,10);\
                 INSERT INTO v VALUES(2,20),(1,10);";
    assert_plan(setup, "SELECT u.x, v.p FROM u CROSS JOIN v");
    assert_rows(setup, "SELECT u.x, v.p FROM u CROSS JOIN v");
    assert_plan_is(
        setup,
        "SELECT u.x, v.p FROM u CROSS JOIN v",
        &["SCAN u", "SCAN v"],
    );
}

#[test]
fn index_not_narrower_stays_plain_scan() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // `v(p)` where `p` is a wide TEXT column: the index on it is NOT strictly
    // narrower than the table's row, so sqlite does NOT use it — plain SCAN.
    let setup = "CREATE TABLE u(x INTEGER PRIMARY KEY, y);\
                 CREATE TABLE v(p TEXT);\
                 CREATE INDEX iv ON v(p);\
                 INSERT INTO u VALUES(1,10),(2,20);\
                 INSERT INTO v VALUES('b'),('a');";
    assert_plan(setup, "SELECT u.x, v.p FROM u CROSS JOIN v");
    assert_rows(setup, "SELECT u.x, v.p FROM u CROSS JOIN v");
    assert_plan_is(
        setup,
        "SELECT u.x, v.p FROM u CROSS JOIN v",
        &["SCAN u", "SCAN v"],
    );
}

#[test]
fn referenced_column_outside_index_stays_plain_scan() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // `v.q` is referenced but is NOT in `iv` → the index does not cover `v`, so
    // sqlite plain-scans it (unchanged), rowid order.
    assert_plan(SETUP, "SELECT u.x, v.p, v.q FROM u CROSS JOIN v");
    assert_rows(SETUP, "SELECT u.x, v.p, v.q FROM u CROSS JOIN v");
    let c = graphite(SETUP, true);
    let plan = graphite_plan(&c, "SELECT u.x, v.p, v.q FROM u CROSS JOIN v");
    assert_eq!(plan, vec!["SCAN u", "SCAN v"]);
}

#[test]
fn wildcard_over_uncovered_table_stays_plain_scan() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // `SELECT *` references every `v` column (incl. uncovered `q`) → plain SCAN.
    assert_plan(SETUP, "SELECT * FROM u CROSS JOIN v");
    assert_rows(SETUP, "SELECT * FROM u CROSS JOIN v");
}
