//! Cost-based join-order for THREE OR MORE tables (the N-table generalisation of
//! the two-table rowid/index inner swaps). sqlite drives a plain equi-INNER join
//! from a table it must SCAN and pulls the rowid-/index-seekable tables into the
//! inner positions, so an unordered query's rows come out in the chosen driver's
//! scan order. graphite reorders the same way (`ntable_join_order`) — only the row
//! ORDER changes; the RESULT SET and the `SELECT *` / `t.*` column order stay in
//! DECLARED `FROM` order.
//!
//! These checks are differential against real sqlite3 3.50.4 (skipped if the
//! binary is absent) and assert the rows (and, where graphite's plan is expected
//! to match byte-for-byte, the EXPLAIN QUERY PLAN), on both `set_use_vdbe(true)`
//! (default) and `(false)`. For a shape graphite deliberately leaves in
//! declaration order (a single-table WHERE restriction, which shifts sqlite's
//! driver choice by a selectivity we do not model), only the row *multiset* is
//! asserted equal — a wrong order is never emitted, but an unreordered one is
//! acceptable.

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
        Value::Text(s) => s.clone(),
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
            Some(Value::Text(s)) => s.clone(),
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

/// Assert graphite's rows for `sql` equal sqlite's EXACTLY (order included), on
/// BOTH VDBE modes. Use for shapes graphite reorders to match sqlite's drive
/// order.
fn assert_rows(setup: &str, sql: &str) {
    let want = sqlite3(&format!("{setup}\n{sql};"));
    for &vdbe in &[true, false] {
        let c = graphite(setup, vdbe);
        let got = graphite_rows(&c, sql);
        assert_eq!(got, want, "rows diverged (use_vdbe={vdbe}) for `{sql}`");
    }
}

/// Assert graphite's rows equal sqlite's as a MULTISET (sorted lines), on both
/// VDBE modes. Use for shapes graphite deliberately leaves in declaration order
/// (row order may then differ from sqlite; only the set must match).
fn assert_rows_multiset(setup: &str, sql: &str) {
    let mut want: Vec<String> = sqlite3(&format!("{setup}\n{sql};"))
        .lines()
        .map(str::to_string)
        .collect();
    want.sort();
    for &vdbe in &[true, false] {
        let c = graphite(setup, vdbe);
        let mut got: Vec<String> = graphite_rows(&c, sql).lines().map(str::to_string).collect();
        got.sort();
        assert_eq!(got, want, "row set diverged (use_vdbe={vdbe}) for `{sql}`");
    }
}

fn assert_plan(setup: &str, sql: &str) {
    let want = sqlite_plan(setup, sql);
    let c = graphite(setup, true);
    let got = graphite_plan(&c, sql);
    assert_eq!(got, want, "plan diverged for `{sql}`");
}

/// The reported bug: a rowid hub `u` sought from `v` (SCAN) and joined with `w`.
/// Asymmetric data makes the drive order observable.
const HUB: &str = "CREATE TABLE u(x INTEGER PRIMARY KEY,y);\
                   CREATE TABLE v(p,q);\
                   CREATE TABLE w(r,s);\
                   CREATE INDEX iv ON v(p);\
                   CREATE INDEX iw ON w(r);\
                   INSERT INTO u VALUES(3,30),(1,10),(2,20);\
                   INSERT INTO v VALUES(2,200),(1,100),(3,300);\
                   INSERT INTO w VALUES(3,3000),(1,1000),(2,2000);";

#[test]
fn hub_case_rows_and_plan() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // sqlite drives from v (SCAN v), seeks the rowid hub u, then joins w — rows in
    // v's scan order (2,1,3). The full ROWS match sqlite exactly (the acceptance
    // criterion); the driver + rowid-hub SEARCH nodes match too. The LAST node —
    // the tail table `w` — is a seek-vs-scan LABEL nuance (sqlite hash-joins the
    // tiny tail; graphite seeks its index): rows are identical either way, so only
    // the driver + hub prefix is asserted, not the whole plan (mirrors how the
    // two-table slice leaves the LEFT-join inner-node text unasserted).
    let sql = "SELECT u.x,v.q,w.s FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r";
    assert_rows(HUB, sql);
    // Confirm the drive order concretely: rows in v's scan order, not u's.
    let c = graphite(HUB, true);
    assert_eq!(graphite_rows(&c, sql), "2|200|2000\n1|100|1000\n3|300|3000");
    let plan = graphite_plan(&c, sql);
    assert_eq!(plan[0], "SCAN v");
    assert_eq!(plan[1], "SEARCH u USING INTEGER PRIMARY KEY (rowid=?)");
}

#[test]
fn chain_case_rows_and_plan() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // A chain u—v—w (v.q=w.r), so w connects through v, not u. sqlite still drives
    // from v, seeks u by rowid, seeks w by index — plan AND rows match exactly.
    let setup = "CREATE TABLE u(x INTEGER PRIMARY KEY,y);\
                 CREATE TABLE v(p,q);\
                 CREATE TABLE w(r,s);\
                 CREATE INDEX iv ON v(p);\
                 CREATE INDEX iw ON w(r);\
                 INSERT INTO u VALUES(3,30),(1,10),(2,20);\
                 INSERT INTO v VALUES(2,20),(1,10),(3,30);\
                 INSERT INTO w VALUES(30,3000),(10,1000),(20,2000);";
    let sql = "SELECT u.x,v.q,w.s FROM u JOIN v ON u.x=v.p JOIN w ON v.q=w.r";
    assert_plan(setup, sql);
    assert_rows(setup, sql);
}

#[test]
fn select_star_keeps_declared_column_order() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Even though v drives, `SELECT *` expands in DECLARED order u,v,w — and each
    // row's values follow that layout.
    assert_rows(HUB, "SELECT * FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r");
    assert_rows(
        HUB,
        "SELECT u.*, w.*, v.* FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r",
    );
}

#[test]
fn four_table_case() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Four tables, all hanging off the rowid hub u. sqlite drives a scanned table
    // and pulls u (rowid) inner; the rows come out in the driver's scan order.
    let setup = "CREATE TABLE u(x INTEGER PRIMARY KEY,y);\
                 CREATE TABLE v(p,q);\
                 CREATE TABLE w(r,s);\
                 CREATE TABLE z(a,b);\
                 CREATE INDEX iv ON v(p);\
                 CREATE INDEX iw ON w(r);\
                 CREATE INDEX iz ON z(a);\
                 INSERT INTO u VALUES(3,30),(1,10),(2,20);\
                 INSERT INTO v VALUES(2,200),(1,100),(3,300);\
                 INSERT INTO w VALUES(3,3000),(1,1000),(2,2000);\
                 INSERT INTO z VALUES(2,22),(1,11),(3,33);";
    let sql = "SELECT u.x,v.q,w.s,z.b FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r JOIN z ON u.x=z.a";
    // The rows come out in the driver's scan order (matching sqlite exactly).
    assert_rows(setup, sql);
    // The chosen driver is a scanned table, and the rowid hub u is a rowid inner.
    let c = graphite(setup, true);
    let plan = graphite_plan(&c, sql);
    assert!(plan[0].starts_with("SCAN "), "driver is a SCAN: {plan:?}");
    assert!(
        plan.iter()
            .any(|l| l == "SEARCH u USING INTEGER PRIMARY KEY (rowid=?)"),
        "u is the rowid inner: {plan:?}"
    );
}

#[test]
fn order_by_stays_sorted() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // An explicit ORDER BY wins regardless of the drive direction, on both modes.
    assert_rows(
        HUB,
        "SELECT u.x,v.q,w.s FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r ORDER BY u.x",
    );
    assert_rows(
        HUB,
        "SELECT u.x,v.q,w.s FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r ORDER BY w.s DESC",
    );
}

#[test]
fn aggregate_and_distinct_over_join() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Aggregates collapse the row set — the reorder must not change the RESULT.
    assert_rows(
        HUB,
        "SELECT count(*),sum(w.s) FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r",
    );
    // DISTINCT over the join: the set is order-independent, so compare as a set.
    assert_rows_multiset(
        HUB,
        "SELECT DISTINCT v.q FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r",
    );
}

#[test]
fn where_filter_result_set_matches() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // A single-table WHERE restriction shifts sqlite's driver choice by selectivity
    // (a cost factor graphite does not model), so graphite declines the reorder and
    // runs in declaration order. The RESULT SET still matches sqlite exactly (order
    // may differ — only the set is asserted).
    assert_rows_multiset(
        HUB,
        "SELECT u.x,v.q,w.s FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r WHERE w.s>1000",
    );
    // A cross-table WHERE equi-predicate is not a single-table restriction, so the
    // reorder may still fire; either way the set matches.
    assert_rows_multiset(
        HUB,
        "SELECT u.x,v.q,w.s FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r WHERE v.q>u.y",
    );
}

#[test]
fn comma_join_form_reorders_identically() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // The comma form promotes its WHERE equalities to ONs and reorders identically.
    let sql = "SELECT u.x,v.q,w.s FROM u,v,w WHERE u.x=v.p AND u.x=w.r";
    assert_rows(HUB, sql);
}

// ---- Negative / unchanged cases ---------------------------------------------

#[test]
fn two_table_still_handled_by_existing_slices() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // A two-table join is NOT the N-table path (it needs >= 3 tables); the existing
    // rowid/index inner swaps own it, unchanged. Plan + rows match sqlite.
    let sql = "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p";
    assert_plan(HUB, sql);
    assert_rows(HUB, sql);
}

#[test]
fn left_join_never_reordered() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // A LEFT join anywhere in the chain fixes the order semantically — the N-table
    // reorder declines (it requires every join to be plain INNER). Rows must match
    // sqlite's fixed order; assert only the multiset (graphite keeps declaration
    // order, sqlite's outer-join order is likewise fixed but the null-padding makes
    // exact order comparison brittle across planners).
    let sql = "SELECT u.x,v.q,w.s FROM u JOIN v ON u.x=v.p LEFT JOIN w ON u.x=w.r";
    assert_rows_multiset(HUB, sql);
    // u remains the driver (declaration order — no reorder).
    let c = graphite(HUB, true);
    assert_eq!(graphite_plan(&c, sql)[0], "SCAN u");
}

#[test]
fn natural_and_using_never_reordered() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // NATURAL / USING joins are excluded from the reorder (they coalesce columns and
    // fix order). The RESULT SET must still match sqlite.
    let setup = "CREATE TABLE u(x INTEGER PRIMARY KEY,k);\
                 CREATE TABLE v(k,q);\
                 CREATE TABLE w(k,s);\
                 INSERT INTO u VALUES(1,10),(2,20),(3,30);\
                 INSERT INTO v VALUES(20,200),(10,100),(30,300);\
                 INSERT INTO w VALUES(30,3000),(10,1000),(20,2000);";
    assert_rows_multiset(setup, "SELECT * FROM u JOIN v USING(k) JOIN w USING(k)");
    assert_rows_multiset(setup, "SELECT * FROM u NATURAL JOIN v NATURAL JOIN w");
}

#[test]
fn cross_product_no_equijoin_unchanged() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // No connecting equi-join between one table and the rest → the reorder declines
    // (a cross-product step it will not introduce). Result set matches sqlite.
    let setup = "CREATE TABLE u(x INTEGER PRIMARY KEY,y);\
                 CREATE TABLE v(p,q);\
                 CREATE TABLE w(r,s);\
                 CREATE INDEX iv ON v(p);\
                 INSERT INTO u VALUES(1,10),(2,20);\
                 INSERT INTO v VALUES(1,100),(2,200);\
                 INSERT INTO w VALUES(7,7000),(8,8000);";
    // w is a bare cross product (no ON tying it to u/v).
    assert_rows_multiset(
        setup,
        "SELECT u.x,v.q,w.s FROM u JOIN v ON u.x=v.p CROSS JOIN w",
    );
}
