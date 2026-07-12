//! Table-qualified rowid (`t.rowid`/`t._rowid_`/`t.oid`) across a join THAT ALSO
//! triggers a cost-based join reorder.
//!
//! A cost-based reorder drives an unordered two-/N-table join from a table sqlite
//! must SCAN and seeks the cheaper table(s) as the inner(s), so the rows come out
//! in the driver's scan order rather than declaration order. The qualified-rowid
//! feature threads a hidden per-table rowid column through the join; these checks
//! confirm the reorder STILL fires (matching sqlite's exact unordered row order)
//! while the rowid alias still resolves — the two features compose.
//!
//! Covered reorder shapes, each selecting/using a qualified rowid with NO
//! `ORDER BY` (so the raw join order is observable):
//! - the two-table **secondary-index-inner swap** (index on the first table's join
//!   column) — the task's headline repro;
//! - the two-table **rowid-inner swap** (the first table's IPK is the join column,
//!   the second is secondary-indexed);
//! - a **three-table** reorder.
//!
//! Each is also re-checked WITH an `ORDER BY` (must be unchanged) and in a
//! non-rowid form (must be unchanged). Differential vs real sqlite3 3.50.4
//! (skipped if the binary is absent), on both `set_use_vdbe` modes.

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

/// graphite's rows for `sql` equal sqlite's EXACTLY (unordered row order included),
/// on BOTH VDBE modes.
fn assert_rows(setup: &str, sql: &str) {
    let want = sqlite3(&format!("{setup}\n{sql};"));
    for &vdbe in &[true, false] {
        let c = graphite(setup, vdbe);
        let got = graphite_rows(&c, sql);
        assert_eq!(got, want, "rows diverged (use_vdbe={vdbe}) for `{sql}`");
    }
}

// ── Two-table secondary-index-inner swap (the task's headline repro). ─────────
// `t.a` is secondary-indexed; sqlite SCANs `u` and SEARCHes `t` by index `i`, so
// the rows come out in `u`'s scan order (m then n), with `t`'s duplicate `a=1`
// rows (rowids 1,3) fanned out under the `a=1` driver row.
const IDX_SETUP: &str = "\
CREATE TABLE t(a,b);\
CREATE INDEX i ON t(a);\
CREATE TABLE u(a,c);\
INSERT INTO t VALUES(1,'x'),(2,'y'),(1,'z');\
INSERT INTO u VALUES(1,'m'),(2,'n');\
";

#[test]
fn index_inner_swap_qualified_rowid() {
    if !have_sqlite3() {
        return;
    }
    // The exact repro from the task: sqlite → 1|m / 3|m / 2|n.
    assert_rows(IDX_SETUP, "SELECT t.rowid,u.c FROM t JOIN u ON t.a=u.a");
    assert_eq!(
        sqlite3(&format!(
            "{IDX_SETUP}\nSELECT t.rowid,u.c FROM t JOIN u ON t.a=u.a;"
        )),
        "1|m\n3|m\n2|n",
        "sqlite's expected order changed; the repro assumption is stale"
    );
    // Both tables' rowids projected (u has a rowid too).
    assert_rows(
        IDX_SETUP,
        "SELECT t.rowid,u.rowid,u.c FROM t JOIN u ON t.a=u.a",
    );
    // A qualified rowid referenced in the WHERE (still the swap; still u-order).
    assert_rows(
        IDX_SETUP,
        "SELECT t.rowid,u.c FROM t JOIN u ON t.a=u.a WHERE t.rowid>0",
    );
    // `_rowid_` / `oid` aliases resolve identically.
    assert_rows(IDX_SETUP, "SELECT t._rowid_,u.c FROM t JOIN u ON t.a=u.a");
    assert_rows(IDX_SETUP, "SELECT t.oid,u.c FROM t JOIN u ON t.a=u.a");
}

#[test]
fn index_inner_swap_qualified_rowid_order_by_unchanged() {
    if !have_sqlite3() {
        return;
    }
    // An explicit ORDER BY re-sorts identically (rowid still resolves).
    assert_rows(
        IDX_SETUP,
        "SELECT t.rowid,u.c FROM t JOIN u ON t.a=u.a ORDER BY t.rowid,u.c",
    );
    assert_rows(
        IDX_SETUP,
        "SELECT t.rowid,u.c FROM t JOIN u ON t.a=u.a ORDER BY u.c,t.rowid DESC",
    );
}

#[test]
fn index_inner_swap_nonrowid_unchanged() {
    if !have_sqlite3() {
        return;
    }
    // The plain (no qualified rowid) swap is byte-identical to before.
    assert_rows(IDX_SETUP, "SELECT t.b,u.c FROM t JOIN u ON t.a=u.a");
    assert_rows(IDX_SETUP, "SELECT * FROM t JOIN u ON t.a=u.a");
}

// ── Two-table rowid-inner swap (first table's IPK is the join column). ────────
// `u.x` is an INTEGER PRIMARY KEY, `v.p` is only secondary-indexed: sqlite SCANs
// `v` and SEARCHes `u` by INTEGER PRIMARY KEY, so rows come out in `v`'s scan
// order (2,1,3).
const ROWID_SETUP: &str = "\
CREATE TABLE u(x INTEGER PRIMARY KEY, y);\
CREATE TABLE v(p, q);\
CREATE INDEX iv ON v(p);\
INSERT INTO u VALUES(3,30),(1,10),(2,20);\
INSERT INTO v VALUES(2,200),(1,100),(3,300);\
";

#[test]
fn rowid_inner_swap_qualified_rowid() {
    if !have_sqlite3() {
        return;
    }
    // `u.rowid` == its IPK; rows in `v`'s scan order (2,1,3).
    assert_rows(ROWID_SETUP, "SELECT u.rowid,v.q FROM u JOIN v ON u.x=v.p");
    // The driver `v`'s rowid also projects; still v-scan order.
    assert_rows(
        ROWID_SETUP,
        "SELECT u.x,v.q,v.rowid FROM u JOIN v ON u.x=v.p",
    );
    // A qualified rowid in the WHERE keeps the swap.
    assert_rows(
        ROWID_SETUP,
        "SELECT u.rowid,v.q FROM u JOIN v ON u.x=v.p WHERE u.rowid>0",
    );
}

#[test]
fn rowid_inner_swap_qualified_rowid_order_by_unchanged() {
    if !have_sqlite3() {
        return;
    }
    assert_rows(
        ROWID_SETUP,
        "SELECT u.rowid,v.q FROM u JOIN v ON u.x=v.p ORDER BY u.rowid",
    );
    assert_rows(
        ROWID_SETUP,
        "SELECT u.rowid,v.q FROM u JOIN v ON u.x=v.p ORDER BY v.q DESC",
    );
}

#[test]
fn rowid_inner_swap_nonrowid_unchanged() {
    if !have_sqlite3() {
        return;
    }
    assert_rows(ROWID_SETUP, "SELECT u.x,v.q FROM u JOIN v ON u.x=v.p");
    assert_rows(ROWID_SETUP, "SELECT * FROM u JOIN v ON u.x=v.p");
}

// ── Three-table reorder. ──────────────────────────────────────────────────────
// Declared order q,p,r; `p.id` is an INTEGER PRIMARY KEY sought from both q and r.
// sqlite drives from a SCANned table and pulls `p` inward, permuting the join.
const N_SETUP: &str = "\
CREATE TABLE p(id INTEGER PRIMARY KEY, pv);\
CREATE TABLE q(qv, pid);\
CREATE TABLE r(rv, pid);\
INSERT INTO p VALUES(1,'p1'),(2,'p2');\
INSERT INTO q VALUES('q1',1),('q2',2);\
INSERT INTO r VALUES('r1',1),('r2',2),('r3',1);\
";

#[test]
fn three_table_reorder_qualified_rowid() {
    if !have_sqlite3() {
        return;
    }
    // `p.rowid` == its IPK; the reordered join's unordered row order must match.
    assert_rows(
        N_SETUP,
        "SELECT p.rowid,q.qv,r.rv FROM q JOIN p ON q.pid=p.id JOIN r ON r.pid=p.id",
    );
    // Every base table's rowid projected.
    assert_rows(
        N_SETUP,
        "SELECT p.rowid,q.rowid,r.rowid,q.qv,r.rv FROM q JOIN p ON q.pid=p.id JOIN r ON r.pid=p.id",
    );
    // A qualified rowid in the WHERE keeps the reorder.
    assert_rows(
        N_SETUP,
        "SELECT p.pv,q.qv,r.rv FROM q JOIN p ON q.pid=p.id JOIN r ON r.pid=p.id WHERE p.rowid>0",
    );
}

#[test]
fn three_table_reorder_qualified_rowid_order_by_unchanged() {
    if !have_sqlite3() {
        return;
    }
    assert_rows(
        N_SETUP,
        "SELECT p.rowid,q.qv,r.rv FROM q JOIN p ON q.pid=p.id JOIN r ON r.pid=p.id \
         ORDER BY p.rowid,q.qv,r.rv",
    );
}

#[test]
fn three_table_reorder_nonrowid_unchanged() {
    if !have_sqlite3() {
        return;
    }
    assert_rows(
        N_SETUP,
        "SELECT p.pv,q.qv,r.rv FROM q JOIN p ON q.pid=p.id JOIN r ON r.pid=p.id",
    );
    assert_rows(
        N_SETUP,
        "SELECT * FROM q JOIN p ON q.pid=p.id JOIN r ON r.pid=p.id",
    );
}
