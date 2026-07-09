//! Cost-based two-table join-order selection against the STAT4 oracle.
//!
//! When BOTH sides of a plain equi-INNER join are cheaply seekable on their join
//! column (rowid IPK or the leading column of a plain secondary index), which one
//! is the *inner* is a cost decision: sqlite drives from the table it must scan
//! the fewest rows of and seeks the other. graphite's two-table swaps
//! (`two_table_rowid_inner_swap` / `two_table_index_inner_swap`) now port the
//! relevant slice of `wherePathSolver` — computing the LogEst path cost each way
//! and flipping the driver only when driving the second table is strictly cheaper.
//!
//! The decision is gated on `sqlite_stat1` being present: WITHOUT `ANALYZE` the
//! plan is byte-identical to the historical (declaration-order) behaviour, so the
//! flip only ever happens where sqlite 3.50.4 also flips. These checks are
//! differential against a STAT4-enabled `sqlite3` (skipped if absent) and assert
//! both the `EXPLAIN QUERY PLAN` and the (drive-order-sensitive) rows.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::path::PathBuf;
use std::process::Command;

/// The STAT4-enabled oracle built from source (see the memory note
/// `sqlite-cost-model-port`). Returns its path if it exists, else `None` (the
/// test then no-ops, exactly like the plain-`sqlite3` differential tests skip
/// when the CLI is absent).
fn oracle() -> Option<PathBuf> {
    // Discovered relative to the scratchpad the task pins; fall back to a couple
    // of conventional spots. Any absent path ⇒ skip.
    let candidates = [
        std::env::var("SQLITE3_ORACLE").ok().map(PathBuf::from),
        std::env::var("TMPDIR").ok().map(|t| {
            PathBuf::from(t).join("sqlite-src/sqlite-amalgamation-3500400/sqlite3-oracle")
        }),
    ];
    candidates.into_iter().flatten().find(|c| c.exists())
}

fn oracle_run(bin: &PathBuf, script: &str) -> String {
    let o = Command::new(bin)
        .arg(":memory:")
        .arg(script)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn oracle_plan(bin: &PathBuf, setup: &str, sql: &str) -> Vec<String> {
    let out = oracle_run(bin, &format!("{setup}\nEXPLAIN QUERY PLAN {sql};"));
    out.lines()
        .filter(|l| *l != "QUERY PLAN")
        .map(|l| l.trim_start_matches(['|', '-', '`', ' ']).to_string())
        .collect()
}

fn fmt(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => {
            if *r == (*r as i64) as f64 {
                format!("{r:.1}")
            } else {
                format!("{r}")
            }
        }
        Value::Text(s) => s.clone(),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
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

fn graphite_rows(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(fmt).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Assert graphite's EQP for `sql` matches the STAT4 oracle exactly.
fn assert_plan(bin: &PathBuf, setup: &str, sql: &str) {
    let want = oracle_plan(bin, setup, sql);
    let c = graphite(setup, true);
    let got = graphite_plan(&c, sql);
    assert_eq!(got, want, "plan diverged for `{sql}`");
}

/// Assert graphite's rows (drive-order-sensitive, exact) match the oracle on both
/// VDBE modes.
fn assert_rows(bin: &PathBuf, setup: &str, sql: &str) {
    let want = oracle_run(bin, &format!("{setup}\n{sql};"));
    for &vdbe in &[true, false] {
        let c = graphite(setup, vdbe);
        let got = graphite_rows(&c, sql);
        assert_eq!(got, want, "rows diverged (use_vdbe={vdbe}) for `{sql}`");
    }
}

/// A huge table (1000 rows) declared FIRST joined to a tiny one (5 rows) declared
/// second, both seekable on their join column. sqlite drives the tiny second
/// table; graphite now flips to match.
const IDX: &str = "CREATE TABLE big(id INTEGER PRIMARY KEY,k,v);\
                   CREATE TABLE small(id INTEGER PRIMARY KEY,k,v);\
                   CREATE INDEX ibk ON big(k);\
                   CREATE INDEX isk ON small(k);\
                   WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<1000)\
                     INSERT INTO big SELECT i,i%50,i FROM c;\
                   WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<5)\
                     INSERT INTO small SELECT i,i,i FROM c;\
                   ANALYZE;";

#[test]
fn both_secondary_index_seekable_drives_smaller() {
    let Some(bin) = oracle() else {
        eprintln!("STAT4 oracle not found; skipping");
        return;
    };
    // Declared big,small: sqlite drives small (SCAN small), seeks big by index.
    let sql = "SELECT * FROM big JOIN small ON big.k=small.k";
    assert_plan(&bin, IDX, sql);
    let c = graphite(IDX, true);
    let plan = graphite_plan(&c, sql);
    assert_eq!(plan[0], "SCAN small", "drives the smaller table: {plan:?}");
    assert!(
        plan[1].starts_with("SEARCH big USING INDEX ibk"),
        "big is the index inner: {plan:?}"
    );
    assert_rows(
        &bin,
        IDX,
        "SELECT big.v,small.v FROM big JOIN small ON big.k=small.k",
    );
}

#[test]
fn already_optimal_declaration_order_not_flipped() {
    let Some(bin) = oracle() else {
        eprintln!("STAT4 oracle not found; skipping");
        return;
    };
    // Declared small,big: driving small is already optimal — no flip, and it
    // matches the oracle (SCAN small, SEARCH big).
    let sql = "SELECT * FROM small JOIN big ON big.k=small.k";
    assert_plan(&bin, IDX, sql);
    let c = graphite(IDX, true);
    assert_eq!(graphite_plan(&c, sql)[0], "SCAN small");
}

#[test]
fn both_rowid_seekable_drives_smaller() {
    let Some(bin) = oracle() else {
        eprintln!("STAT4 oracle not found; skipping");
        return;
    };
    // Both join columns are the rowid IPK — the both-rowid case. sqlite drives the
    // smaller (second) table and seeks the first by rowid.
    let sql = "SELECT * FROM big JOIN small ON big.id=small.id";
    assert_plan(&bin, IDX, sql);
    let c = graphite(IDX, true);
    let plan = graphite_plan(&c, sql);
    assert_eq!(plan[0], "SCAN small", "drives the smaller table: {plan:?}");
    assert_eq!(plan[1], "SEARCH big USING INTEGER PRIMARY KEY (rowid=?)");
    assert_rows(
        &bin,
        IDX,
        "SELECT big.v,small.v FROM big JOIN small ON big.id=small.id",
    );
}

#[test]
fn comma_join_form_flips_identically() {
    let Some(bin) = oracle() else {
        eprintln!("STAT4 oracle not found; skipping");
        return;
    };
    // The comma form promotes its WHERE equality to an ON and flips the same way.
    let sql = "SELECT * FROM big,small WHERE big.k=small.k";
    assert_plan(&bin, IDX, sql);
    assert_rows(
        &bin,
        IDX,
        "SELECT big.v,small.v FROM big,small WHERE big.k=small.k",
    );
}

#[test]
fn no_analyze_keeps_declaration_order() {
    let Some(bin) = oracle() else {
        eprintln!("STAT4 oracle not found; skipping");
        return;
    };
    // WITHOUT ANALYZE there are no stats, so the cost swap declines and the plan is
    // the historical declaration-order one — which is ALSO what sqlite emits with
    // no stats. Both agree on driving the declared-first table.
    let setup = "CREATE TABLE big(id INTEGER PRIMARY KEY,k,v);\
                 CREATE TABLE small(id INTEGER PRIMARY KEY,k,v);\
                 CREATE INDEX ibk ON big(k);\
                 CREATE INDEX isk ON small(k);\
                 WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<1000)\
                   INSERT INTO big SELECT i,i%50,i FROM c;\
                 WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<5)\
                   INSERT INTO small SELECT i,i,i FROM c;";
    let sql = "SELECT * FROM big JOIN small ON big.k=small.k";
    assert_plan(&bin, setup, sql);
    let c = graphite(setup, true);
    assert_eq!(
        graphite_plan(&c, sql)[0],
        "SCAN big",
        "no stats ⇒ declaration order drives"
    );
}

/// Three tables all seekable on the join column but of very different sizes
/// (u=1000, v=10, w=500), all hanging off the rowid hub u. sqlite drives the
/// SMALLEST scanned table (v) and pulls the others inner; graphite's N-table
/// order now selects the same driver by LogEst path cost.
const HUB3: &str = "CREATE TABLE u(x INTEGER PRIMARY KEY,y);\
                    CREATE TABLE v(p INTEGER PRIMARY KEY,q);\
                    CREATE TABLE w(r INTEGER PRIMARY KEY,s);\
                    WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<1000)\
                      INSERT INTO u SELECT i,i FROM c;\
                    WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<10)\
                      INSERT INTO v SELECT i,i FROM c;\
                    WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<500)\
                      INSERT INTO w SELECT i,i FROM c;\
                    ANALYZE;";

#[test]
fn three_table_drives_smallest_by_cost() {
    let Some(bin) = oracle() else {
        eprintln!("STAT4 oracle not found; skipping");
        return;
    };
    // Declared u,v,w — but v (10 rows) is the cheapest driver. The plan matches the
    // oracle, and the rows come out in v's drive order.
    let sql = "SELECT * FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r";
    assert_plan(&bin, HUB3, sql);
    let c = graphite(HUB3, true);
    assert_eq!(
        graphite_plan(&c, sql)[0],
        "SCAN v",
        "drives the smallest table"
    );
    assert_rows(
        &bin,
        HUB3,
        "SELECT u.x,v.q,w.s FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r",
    );
}

#[test]
fn three_table_no_analyze_declaration_order() {
    let Some(bin) = oracle() else {
        eprintln!("STAT4 oracle not found; skipping");
        return;
    };
    // Same shape without ANALYZE: no stats ⇒ the LogEst driver key is unavailable
    // for every candidate, so the historical coarse ordering (declaration-order
    // driver) stands — matching sqlite with no stats.
    let setup = "CREATE TABLE u(x INTEGER PRIMARY KEY,y);\
                 CREATE TABLE v(p INTEGER PRIMARY KEY,q);\
                 CREATE TABLE w(r INTEGER PRIMARY KEY,s);\
                 WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<1000)\
                   INSERT INTO u SELECT i,i FROM c;\
                 WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<10)\
                   INSERT INTO v SELECT i,i FROM c;\
                 WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<500)\
                   INSERT INTO w SELECT i,i FROM c;";
    let sql = "SELECT u.x,v.q,w.s FROM u JOIN v ON u.x=v.p JOIN w ON u.x=w.r";
    // Result set matches sqlite (order may differ without stats — assert as a set).
    let mut want: Vec<String> = oracle_run(&bin, &format!("{setup}\n{sql};"))
        .lines()
        .map(str::to_string)
        .collect();
    want.sort();
    for &vdbe in &[true, false] {
        let c = graphite(setup, vdbe);
        let mut got: Vec<String> = graphite_rows(&c, sql).lines().map(str::to_string).collect();
        got.sort();
        assert_eq!(got, want, "row set diverged (use_vdbe={vdbe})");
    }
}

#[test]
fn one_side_only_seekable_unchanged() {
    let Some(bin) = oracle() else {
        eprintln!("STAT4 oracle not found; skipping");
        return;
    };
    // Only big.k is indexed; small.k is not seekable. This is NOT the both-seekable
    // case — the existing forward path already makes the seekable side the inner,
    // and that matches the oracle regardless of size.
    let setup = "CREATE TABLE big(id INTEGER PRIMARY KEY,k,v);\
                 CREATE TABLE small(id INTEGER PRIMARY KEY,k,v);\
                 CREATE INDEX ibk ON big(k);\
                 WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<1000)\
                   INSERT INTO big SELECT i,i%50,i FROM c;\
                 WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<5)\
                   INSERT INTO small SELECT i,i,i FROM c;\
                 ANALYZE;";
    assert_plan(&bin, setup, "SELECT * FROM big JOIN small ON big.k=small.k");
    assert_plan(&bin, setup, "SELECT * FROM small JOIN big ON big.k=small.k");
}
