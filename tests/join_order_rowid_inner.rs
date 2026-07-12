//! Cost-based join-order (first slice): a two-table equi-join is driven from the
//! table that leaves the *cheaper* seek on the inner side. When `from.first`'s
//! join column IS its own rowid / INTEGER PRIMARY KEY and the second table's join
//! column is only secondary-indexed, sqlite makes the rowid-seekable table the
//! inner one — it SCANs the second table and SEARCHes the first by INTEGER PRIMARY
//! KEY. That changes the observable ROW ORDER of an unordered query (rows come out
//! in the second table's scan order), not just the plan.
//!
//! These checks are differential against real sqlite3 3.50.4 (skipped if the
//! binary is absent) and assert BOTH the plan (EXPLAIN QUERY PLAN) and the rows,
//! on both `set_use_vdbe(true)` (default) and `(false)`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

/// Run `sql` under real sqlite3 in `-list` mode (default separator `|`), returning
/// the trimmed stdout. The whole script is fed on one connection.
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

/// Format one graphite value the way sqlite3's `-list` mode prints it.
fn fmt(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => {
            // sqlite prints reals with %!.15g-ish; integers-as-real keep `.0`.
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

/// Graphite rows joined the `-list` way: `|` between columns, `\n` between rows.
fn graphite_rows(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(fmt).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Graphite EXPLAIN QUERY PLAN rendered as sqlite renders it: the tree lines only
/// (graphite emits the `detail` in the last column).
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

/// sqlite EXPLAIN QUERY PLAN detail lines (strip the tree-drawing prefix so we
/// compare just the `SCAN`/`SEARCH …` text).
fn sqlite_plan(setup: &str, sql: &str) -> Vec<String> {
    let out = sqlite3(&format!("{setup}\nEXPLAIN QUERY PLAN {sql};"));
    out.lines()
        .filter(|l| *l != "QUERY PLAN")
        .map(|l| l.trim_start_matches(['|', '-', '`', ' ']).to_string())
        .collect()
}

/// Build a graphite connection from `setup`, at the given VDBE mode.
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

const SETUP: &str = "CREATE TABLE u(x INTEGER PRIMARY KEY, y);\
                     CREATE TABLE v(p, q);\
                     CREATE INDEX iv ON v(p);\
                     INSERT INTO u VALUES(3,30),(1,10),(2,20);\
                     INSERT INTO v VALUES(2,200),(1,100),(3,300);";

#[test]
fn bug_case_explicit_join() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // The reported bug: rows must come out in `v`'s scan order (2,1,3), and the
    // plan must SCAN v + SEARCH u USING INTEGER PRIMARY KEY.
    assert_plan(SETUP, "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p");
    assert_rows(SETUP, "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p");
}

#[test]
fn bug_case_comma_join() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // The comma form is promoted to the same ON and reorders identically.
    assert_plan(SETUP, "SELECT u.x,v.q FROM u,v WHERE u.x=v.p");
    assert_rows(SETUP, "SELECT u.x,v.q FROM u,v WHERE u.x=v.p");
}

#[test]
fn select_star_keeps_declared_column_order() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Even though `v` drives, `SELECT *` expands in DECLARED order: u.x,u.y,v.p,v.q.
    assert_rows(SETUP, "SELECT * FROM u JOIN v ON u.x=v.p");
    // `u.*` then `v.*` explicitly.
    assert_rows(SETUP, "SELECT u.*, v.* FROM u JOIN v ON u.x=v.p");
}

#[test]
fn many_to_one_second_table_duplicates() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // `v` has several rows per `p`: each drives one seek into `u`, so every v row
    // matching a u rowid is emitted, in v's scan order.
    let setup = "CREATE TABLE u(x INTEGER PRIMARY KEY, y);\
                 CREATE TABLE v(p, q);\
                 CREATE INDEX iv ON v(p);\
                 INSERT INTO u VALUES(1,10),(2,20);\
                 INSERT INTO v VALUES(2,'a'),(1,'b'),(2,'c'),(1,'d'),(2,'e');";
    assert_plan(setup, "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p");
    assert_rows(setup, "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p");
}

#[test]
fn where_filter_on_either_side() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    assert_rows(
        SETUP,
        "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p WHERE u.y>10",
    );
    assert_rows(
        SETUP,
        "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p WHERE v.q<300",
    );
}

#[test]
fn explicit_order_by_still_sorts() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // An explicit ORDER BY must win regardless of the drive direction.
    assert_rows(
        SETUP,
        "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p ORDER BY u.x",
    );
    assert_rows(
        SETUP,
        "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p ORDER BY v.q DESC",
    );
}

// ---- Negative / unchanged cases ---------------------------------------------

#[test]
fn left_join_order_is_fixed_not_reordered() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // A LEFT JOIN's outer order is semantically fixed — never reordered. Rows must
    // stay in `u`'s scan order (the swap declines LEFT joins). Plan is asserted to
    // still SCAN u (drive from from.first) — the inner-node text is a pre-existing
    // graphite/sqlite LEFT-JOIN-index-rendering nuance orthogonal to this change,
    // so we assert only that `u` remains the driver, plus the full rows.
    let c = graphite(SETUP, true);
    assert_eq!(
        graphite_plan(&c, "SELECT u.x,v.q FROM u LEFT JOIN v ON u.x=v.p")[0],
        "SCAN u",
    );
    assert_rows(SETUP, "SELECT u.x,v.q FROM u LEFT JOIN v ON u.x=v.p");
}

#[test]
fn three_table_join_unchanged() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // More than two tables: the reorder never fires (rule requires exactly two).
    let setup = "CREATE TABLE u(x INTEGER PRIMARY KEY, y);\
                 CREATE TABLE v(p, q);\
                 CREATE INDEX iv ON v(p);\
                 CREATE TABLE w(m, n);\
                 INSERT INTO u VALUES(3,30),(1,10),(2,20);\
                 INSERT INTO v VALUES(2,200),(1,100),(3,300);\
                 INSERT INTO w VALUES(1,'x'),(2,'y'),(3,'z');";
    // Result set must still match sqlite (order may follow either planner, so we
    // sort both sides before comparing).
    let sql = "SELECT u.x,v.q,w.n FROM u JOIN v ON u.x=v.p JOIN w ON v.p=w.m ORDER BY u.x";
    assert_rows(setup, sql);
}

#[test]
fn both_rowid_unchanged() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Both join columns are their own tables' rowid IPK — ambiguous which is the
    // cheaper inner, so the swap declines and order stays as sqlite plans it.
    let setup = "CREATE TABLE u(x INTEGER PRIMARY KEY, y);\
                 CREATE TABLE v(p INTEGER PRIMARY KEY, q);\
                 INSERT INTO u VALUES(3,30),(1,10),(2,20);\
                 INSERT INTO v VALUES(2,200),(1,100),(3,300);";
    assert_plan(setup, "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p");
    assert_rows(setup, "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p");
}

#[test]
fn neither_rowid_unchanged() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Neither join column is a rowid IPK — the swap's precondition (from.first's
    // column IS its rowid) is unmet, so nothing reorders.
    let setup = "CREATE TABLE u(x, y);\
                 CREATE TABLE v(p, q);\
                 CREATE INDEX iv ON v(p);\
                 INSERT INTO u VALUES(3,30),(1,10),(2,20);\
                 INSERT INTO v VALUES(2,200),(1,100),(3,300);";
    assert_plan(setup, "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p");
    assert_rows(setup, "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p");
}

#[test]
fn first_already_rowid_inner_optimal_unchanged() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Here the SECOND table (`v`) is the rowid-seekable one and `u`'s join column
    // is only secondary-indexed. Driving from `from.first` (`u`) already seeks the
    // rowid inner (`v`) — the optimal drive — so the swap must NOT fire (it only
    // fires when from.first is the rowid side): `u` stays the driver and `v` the
    // rowid-sought inner.
    //
    // The driver `u` is itself covering-scanned via `iu` (only `u.x`, in the index,
    // is referenced from `u`; `iu` is strictly narrower than the `u(x,y)` row), so
    // `u`'s rows are visited in `x`-key order — the join-table covering-scan slice.
    // Both plan and rows now match sqlite exactly on both VDBE modes.
    let setup = "CREATE TABLE u(x, y);\
                 CREATE INDEX iu ON u(x);\
                 CREATE TABLE v(p INTEGER PRIMARY KEY, q);\
                 INSERT INTO u VALUES(3,30),(1,10),(2,20);\
                 INSERT INTO v VALUES(2,200),(1,100),(3,300);";
    let sql = "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p";
    assert_plan(setup, sql);
    assert_rows(setup, sql);
    let c = graphite(setup, true);
    let plan = graphite_plan(&c, sql);
    assert_eq!(
        plan[0], "SCAN u USING COVERING INDEX iu",
        "u stays the driver, covering-scanned via iu"
    );
    assert_eq!(plan[1], "SEARCH v USING INTEGER PRIMARY KEY (rowid=?)");
    // Rows follow `u`'s covering-index (x-key) order (1,2,3), matching sqlite.
    assert_eq!(graphite_rows(&c, sql), "1|100\n2|200\n3|300");
}
