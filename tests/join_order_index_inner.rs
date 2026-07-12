//! Cost-based join-order (secondary-index-inner slice): a two-table equi-join is
//! driven from the table that leaves the *cheaper* seek on the inner side. When
//! `from.first`'s join column is the LEADING column of a usable secondary index
//! (but NOT its rowid IPK) and the second table's join column is NOT seekable at
//! all, sqlite makes the index-seekable table the inner one — it SCANs the second
//! table and SEARCHes `from.first` by that index. That changes the observable ROW
//! ORDER of an unordered query (rows come out in the second table's scan order),
//! not just the plan.
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

/// Graphite EXPLAIN QUERY PLAN rendered as sqlite renders it (detail column only).
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

/// sqlite EXPLAIN QUERY PLAN detail lines (strip the tree-drawing prefix).
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

// `b` is `from.first`, indexed on its join column `p` via `ib`; `a` is the
// non-seekable other table (no index / rowid on `m`). Asymmetric data so the
// two possible drive orders are distinguishable.
const SETUP: &str = "CREATE TABLE a(m, n);\
                     CREATE TABLE b(p, q);\
                     CREATE INDEX ib ON b(p);\
                     INSERT INTO a VALUES(3,30),(1,10),(2,20);\
                     INSERT INTO b VALUES(2,222),(1,111),(3,333);";

#[test]
fn bug_case_explicit_join() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // The reported bug: drive from `a` (SCAN a) and seek `b` via `ib`, so rows come
    // out in `a`'s scan order (3,1,2) — plan AND rows must match sqlite.
    assert_plan(SETUP, "SELECT * FROM b JOIN a ON a.m=b.p");
    assert_rows(SETUP, "SELECT * FROM b JOIN a ON a.m=b.p");
}

#[test]
fn bug_case_comma_join() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // The comma form is promoted to the same ON and reorders identically.
    assert_plan(SETUP, "SELECT * FROM b, a WHERE a.m=b.p");
    assert_rows(SETUP, "SELECT * FROM b, a WHERE a.m=b.p");
}

#[test]
fn select_star_keeps_declared_column_order() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Even though `a` drives, `SELECT *` expands in DECLARED order: b.p,b.q,a.m,a.n.
    assert_rows(SETUP, "SELECT * FROM b JOIN a ON a.m=b.p");
    // `b.*` then `a.*` explicitly.
    assert_rows(SETUP, "SELECT b.*, a.* FROM b JOIN a ON a.m=b.p");
    // Explicit reordered column list still projects correctly.
    assert_rows(SETUP, "SELECT a.n, b.p, a.m FROM b JOIN a ON a.m=b.p");
}

#[test]
fn covering_index_when_only_indexed_column_needed() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Only `b.p` (in `ib`) is needed from `b`, so the seek is COVERING: sqlite (and
    // graphite) render `SEARCH b USING COVERING INDEX ib (p=?)`.
    assert_plan(SETUP, "SELECT b.p FROM b JOIN a ON a.m=b.p");
    assert_rows(SETUP, "SELECT b.p FROM b JOIN a ON a.m=b.p");
    // Also covering when only the join column is referenced via `a`.
    assert_plan(SETUP, "SELECT a.m FROM b JOIN a ON a.m=b.p");
}

#[test]
fn non_unique_index_fans_out_in_index_order() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // `b` has several rows per `p`: each `a` driver row seeks `b` and every matching
    // b row is emitted, in the index's order, before the next driver row.
    let setup = "CREATE TABLE a(m, n);\
                 CREATE TABLE b(p, q);\
                 CREATE INDEX ib ON b(p);\
                 INSERT INTO a VALUES(3,30),(1,10);\
                 INSERT INTO b VALUES(3,301),(1,101),(3,302),(1,102);";
    assert_plan(setup, "SELECT * FROM b JOIN a ON a.m=b.p");
    assert_rows(setup, "SELECT * FROM b JOIN a ON a.m=b.p");
}

#[test]
fn where_filter_on_either_side() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    assert_plan(SETUP, "SELECT * FROM b JOIN a ON a.m=b.p WHERE b.q>150");
    assert_rows(SETUP, "SELECT * FROM b JOIN a ON a.m=b.p WHERE b.q>150");
    assert_rows(SETUP, "SELECT * FROM b JOIN a ON a.m=b.p WHERE a.n=30");
}

#[test]
fn explicit_order_by_still_sorts() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // An explicit ORDER BY must win regardless of the drive direction (and the VDBE
    // may run these directly, since the order is then observable-invariant).
    assert_rows(SETUP, "SELECT * FROM b JOIN a ON a.m=b.p ORDER BY b.q");
    assert_rows(SETUP, "SELECT * FROM b JOIN a ON a.m=b.p ORDER BY a.m DESC");
}

// ---- Negative / unchanged cases ---------------------------------------------

#[test]
fn second_table_indexed_forward_seek_not_swapped() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // The second table (`a`) is ALSO indexed on its join column — the existing
    // forward index-seek path already makes it the inner, so the swap declines and
    // `b` stays the driver (matching sqlite).
    let setup = "CREATE TABLE a(m, n);\
                 CREATE TABLE b(p, q);\
                 CREATE INDEX ib ON b(p);\
                 CREATE INDEX ia ON a(m);\
                 INSERT INTO a VALUES(3,30),(1,10),(2,20);\
                 INSERT INTO b VALUES(2,222),(1,111),(3,333);";
    assert_plan(setup, "SELECT * FROM b JOIN a ON a.m=b.p");
    assert_rows(setup, "SELECT * FROM b JOIN a ON a.m=b.p");
}

#[test]
fn second_table_rowid_forward_seek_not_swapped() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // The second table (`a`) is rowid-seekable on its join column — the existing
    // forward rowid-seek path already makes it the inner, so nothing reorders.
    let setup = "CREATE TABLE a(m INTEGER PRIMARY KEY, n);\
                 CREATE TABLE b(p, q);\
                 CREATE INDEX ib ON b(p);\
                 INSERT INTO a VALUES(3,30),(1,10),(2,20);\
                 INSERT INTO b VALUES(2,222),(1,111),(3,333);";
    assert_plan(setup, "SELECT * FROM b JOIN a ON a.m=b.p");
    assert_rows(setup, "SELECT * FROM b JOIN a ON a.m=b.p");
}

#[test]
fn first_table_rowid_uses_rowid_slice_not_index_swap() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // `from.first`'s join column IS its rowid IPK — that is the *rowid* slice's job
    // (`SEARCH b USING INTEGER PRIMARY KEY`), NOT this index swap. Both slices must
    // not fire together; the result matches sqlite either way.
    let setup = "CREATE TABLE b(p INTEGER PRIMARY KEY, q);\
                 CREATE TABLE a(m, n);\
                 INSERT INTO b VALUES(2,222),(1,111),(3,333);\
                 INSERT INTO a VALUES(3,30),(1,10),(2,20);";
    assert_plan(setup, "SELECT * FROM b JOIN a ON a.m=b.p");
    assert_rows(setup, "SELECT * FROM b JOIN a ON a.m=b.p");
    // The inner node must be the rowid seek, not an index seek.
    let c = graphite(setup, true);
    let plan = graphite_plan(&c, "SELECT * FROM b JOIN a ON a.m=b.p");
    assert_eq!(plan[0], "SCAN a");
    assert_eq!(plan[1], "SEARCH b USING INTEGER PRIMARY KEY (rowid=?)");
}

#[test]
fn left_join_not_reordered() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // A LEFT JOIN's outer order is semantically fixed — the swap declines LEFT
    // joins, so `b` stays the driver. Plan and rows both match sqlite.
    assert_plan(SETUP, "SELECT * FROM b LEFT JOIN a ON a.m=b.p");
    assert_rows(SETUP, "SELECT * FROM b LEFT JOIN a ON a.m=b.p");
    let c = graphite(SETUP, true);
    assert_eq!(
        graphite_plan(&c, "SELECT * FROM b LEFT JOIN a ON a.m=b.p")[0],
        "SCAN b"
    );
}

#[test]
fn neither_seekable_unchanged() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Neither join column is seekable (no index on either side) — the swap's
    // precondition (from.first's column is index-leading) is unmet, so the plan
    // stays graphite's nested/hash join, matching sqlite.
    let setup = "CREATE TABLE a(m, n);\
                 CREATE TABLE b(p, q);\
                 INSERT INTO a VALUES(3,30),(1,10);\
                 INSERT INTO b VALUES(1,111),(3,333);";
    assert_plan(setup, "SELECT * FROM b JOIN a ON a.m=b.p");
    assert_rows(setup, "SELECT * FROM b JOIN a ON a.m=b.p");
}

#[test]
fn three_table_join_unchanged() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // More than two tables: the reorder never fires (rule requires exactly two). We
    // assert only the row *set* (via an explicit ORDER BY), since the 3-table drive
    // order is a separate, pre-existing planner concern.
    let setup = "CREATE TABLE a(m, n);\
                 CREATE TABLE b(p, q);\
                 CREATE TABLE c(x, y);\
                 CREATE INDEX ib ON b(p);\
                 INSERT INTO a VALUES(3,30),(1,10);\
                 INSERT INTO b VALUES(1,111),(3,333);\
                 INSERT INTO c VALUES(1,9),(3,8);";
    let sql = "SELECT b.p,a.n,c.y FROM b JOIN a ON a.m=b.p JOIN c ON c.x=a.m ORDER BY b.p";
    assert_rows(setup, sql);
}
