//! Track B (EQP): the trailing `USE TEMP B-TREE FOR {DISTINCT,GROUP BY,ORDER BY}`
//! nodes over a two-table INNER join. The result rows already matched sqlite in
//! every case; these are EQP-only fixes bringing the *plan node list* into lockstep
//! with sqlite3 3.50.4.
//!
//! Two gaps are closed:
//!
//!  * **DISTINCT / GROUP BY over a join** — sqlite appends a root-level
//!    `USE TEMP B-TREE FOR {DISTINCT,GROUP BY}` after the join's SCAN/SEARCH nodes
//!    whenever the driver's scan order does not already *cluster* the key. graphite
//!    previously omitted it for every join. It is now emitted — and *elided* exactly
//!    when the key is a leading prefix of the driver's scan order (e.g. `DISTINCT
//!    v.p` / `GROUP BY v.p` where `v` is the covering-index-scanned driver on `p`),
//!    matching sqlite.
//!
//!  * **ORDER BY on the driver's own scan order** — when the join's outer DRIVER is
//!    scanned in an order that already satisfies the `ORDER BY` (its rowid order for
//!    an INTEGER-PRIMARY-KEY driver, or its covering-index leading-column order),
//!    sqlite emits NO `USE TEMP B-TREE FOR ORDER BY`; graphite previously added a
//!    redundant one. It is now elided when the ORDER BY terms are a prefix of the
//!    driver's scan order (both ASC and DESC — the index / rowid walks either way),
//!    and a *partial* prefix reports only the trailing unsupplied terms
//!    (`LAST TERM OF ORDER BY`). An ORDER BY touching the seeked inner, or a
//!    non-driver-order column, keeps its full sorter.
//!
//! A combined GROUP BY + ORDER BY folds the sort into the grouping b-tree exactly
//! when every ORDER BY term is the GROUP BY key (any direction); otherwise both
//! nodes stand.
//!
//! Every case asserts the full plan node list AND the result rows, differential vs
//! sqlite3 3.50.4, on both VDBE modes (`set_use_vdbe(true)`/`(false)`).
//!
//! Out of scope (a separate covering-index-*selection* divergence, not a
//! trailing-node one): `ORDER BY v.p, v.q` where sqlite picks a wider covering
//! index `(p,q)` for the driver but graphite plain-scans — graphite's plan is
//! internally consistent (a plain `SCAN v` supplies no order, so it full-sorts), and
//! the rows still match; the divergence is which index the driver reads, unrelated
//! to this slice.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn norm(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).trim_end())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn graphite(ddl: &str, use_vdbe: bool) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.set_use_vdbe(use_vdbe);
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

fn g_eqp(c: &Connection, q: &str) -> String {
    let rows = c.query(&format!("EXPLAIN QUERY PLAN {q}")).unwrap().rows;
    rows.iter()
        .filter_map(|r| match r.last() {
            Some(Value::Text(s)) => Some(String::from(s.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn render_rows(rows: &[Vec<Value>]) -> String {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => {
                        let s = format!("{f}");
                        if s.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                            format!("{s}.0")
                        } else {
                            s
                        }
                    }
                    Value::Text(s) => String::from(s.as_str()),
                    Value::Blob(_) => "<blob>".to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sqlite_eqp(ddl: &str, q: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{ddl} EXPLAIN QUERY PLAN {q};"))
        .output()
        .unwrap();
    norm(&String::from_utf8_lossy(&o.stdout))
}

fn sqlite_rows(ddl: &str, q: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{ddl} {q};"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

/// Assert graphite's plan AND rows equal sqlite's, on both VDBE modes. The plan is
/// asserted to equal the given `expect` string too (a self-documenting oracle of
/// the exact node list this slice produces).
fn check(ddl: &str, q: &str, expect: &str) {
    let want_eqp = sqlite_eqp(ddl, q);
    let want_rows = sqlite_rows(ddl, q);
    assert_eq!(want_eqp, expect, "sqlite plan != expected for `{q}`");
    for &vdbe in &[true, false] {
        let c = graphite(ddl, vdbe);
        assert_eq!(
            g_eqp(&c, q),
            want_eqp,
            "EQP diverged (use_vdbe={vdbe}) for `{q}`"
        );
        let got = render_rows(&c.query(q).unwrap().rows);
        assert_eq!(got, want_rows, "rows diverged (use_vdbe={vdbe}) for `{q}`");
    }
}

// `u` is a rowid table (IPK `x`); `v` is a rowid-less pair with a covering index
// `iv` on `p`. `FROM u JOIN v ON u.x=v.p` cost-swaps to drive `v` (scanned via the
// covering index `iv`, in `p` order) and seek `u` by rowid.
const D: &str = "CREATE TABLE u(x INTEGER PRIMARY KEY,y); CREATE TABLE v(p,q); \
    CREATE INDEX iv ON v(p); \
    INSERT INTO u VALUES(3,30),(1,10),(2,20); \
    INSERT INTO v VALUES(2,200),(1,100),(3,300);";
// `w` is a rowid table (IPK `a`) used to exercise a rowid-*seeked* inner: `FROM w
// JOIN v ON w.a=v.p` also drives `v` and seeks `w`, so a DISTINCT/GROUP BY/ORDER BY
// on `w.a` (the seeked inner) can NOT be clustered by the driver scan.
const DW: &str = "CREATE TABLE v(p,q); CREATE INDEX iv ON v(p); \
    CREATE TABLE w(a INTEGER PRIMARY KEY,b); \
    INSERT INTO v VALUES(2,200),(1,100),(3,300); \
    INSERT INTO w VALUES(1,10),(2,20),(3,30);";

/// DISTINCT over a join whose key is on the SEEKED inner (not the driver order):
/// the temp b-tree is emitted after the join nodes.
#[test]
fn distinct_over_join_seek_inner() {
    if !have_sqlite() {
        return;
    }
    check(
        D,
        "SELECT DISTINCT u.y FROM u JOIN v ON u.x=v.p",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?) | \
         USE TEMP B-TREE FOR DISTINCT",
    );
    // A two-column DISTINCT spanning both sides also spills (driver order clusters
    // only the leading driver column).
    check(
        D,
        "SELECT DISTINCT u.x,v.q FROM u JOIN v ON u.x=v.p",
        "SCAN v | SEARCH u USING INTEGER PRIMARY KEY (rowid=?) | USE TEMP B-TREE FOR DISTINCT",
    );
}

/// DISTINCT on the driver's own covering-index key column is already clustered by
/// the driver scan — sqlite (and now graphite) emit NO b-tree.
#[test]
fn distinct_on_driver_key_elided() {
    if !have_sqlite() {
        return;
    }
    check(
        D,
        "SELECT DISTINCT v.p FROM u JOIN v ON u.x=v.p",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?)",
    );
}

/// GROUP BY over a join spills through the grouping b-tree; +HAVING is the same.
#[test]
fn group_by_over_join() {
    if !have_sqlite() {
        return;
    }
    check(
        D,
        "SELECT u.x, count(*) FROM u JOIN v ON u.x=v.p GROUP BY u.x",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?) | \
         USE TEMP B-TREE FOR GROUP BY",
    );
    check(
        D,
        "SELECT u.x, count(*) FROM u JOIN v ON u.x=v.p GROUP BY u.x HAVING count(*)>0",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?) | \
         USE TEMP B-TREE FOR GROUP BY",
    );
    // GROUP BY on the driver key column is clustered → no node.
    check(
        D,
        "SELECT v.p,count(*) FROM u JOIN v ON u.x=v.p GROUP BY v.p",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?)",
    );
    // GROUP BY on the rowid-seeked inner (`w.a`, driver is `v`) is NOT clustered.
    check(
        DW,
        "SELECT w.a,count(*) FROM w JOIN v ON w.a=v.p GROUP BY w.a",
        "SCAN v USING COVERING INDEX iv | SEARCH w USING INTEGER PRIMARY KEY (rowid=?) | \
         USE TEMP B-TREE FOR GROUP BY",
    );
}

/// ORDER BY the driver's covering-index leading column — elided both ASC and DESC.
#[test]
fn order_by_driver_covering_key_elided() {
    if !have_sqlite() {
        return;
    }
    check(
        D,
        "SELECT u.x, v.p FROM u JOIN v ON u.x=v.p ORDER BY v.p",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?)",
    );
    check(
        D,
        "SELECT u.x, v.p FROM u JOIN v ON u.x=v.p ORDER BY v.p ASC",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?)",
    );
    check(
        D,
        "SELECT u.x, v.p FROM u JOIN v ON u.x=v.p ORDER BY v.p DESC",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?)",
    );
}

/// ORDER BY the rowid-order driver's IPK column — elided (rowid scan order).
#[test]
fn order_by_driver_rowid_elided() {
    if !have_sqlite() {
        return;
    }
    check(
        DW,
        "SELECT w.a FROM w JOIN v ON w.a=v.p ORDER BY w.a",
        // Cost-swap drives `v`, seeks `w`; `w.a` is the seeked inner here, NOT the
        // driver — so sqlite KEEPS the sorter. Verifies the seek-inner negative.
        "SCAN v USING COVERING INDEX iv | SEARCH w USING INTEGER PRIMARY KEY (rowid=?) | \
         USE TEMP B-TREE FOR ORDER BY",
    );
}

/// ORDER BY on the SEEKED inner column keeps its full sorter — its rows are
/// per-driver-row, not globally ordered.
#[test]
fn order_by_seek_inner_keeps_sorter() {
    if !have_sqlite() {
        return;
    }
    // `u.x` is the rowid-seeked inner (driver is `v`).
    check(
        D,
        "SELECT u.x, v.p FROM u JOIN v ON u.x=v.p ORDER BY u.x",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?) | \
         USE TEMP B-TREE FOR ORDER BY",
    );
    // `v.q` is a driver column NOT in the covering index → driver plain-scans (no
    // order), sorter stays.
    check(
        D,
        "SELECT u.x, v.q FROM u JOIN v ON u.x=v.p ORDER BY v.q",
        "SCAN v | SEARCH u USING INTEGER PRIMARY KEY (rowid=?) | USE TEMP B-TREE FOR ORDER BY",
    );
}

/// A partial ORDER BY: the driver supplies the leading term (`v.p`), the trailing
/// term is on the seeked inner → `LAST TERM OF ORDER BY`.
#[test]
fn order_by_partial_prefix() {
    if !have_sqlite() {
        return;
    }
    check(
        D,
        "SELECT u.x, v.p FROM u JOIN v ON u.x=v.p ORDER BY v.p, u.x",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?) | \
         USE TEMP B-TREE FOR LAST TERM OF ORDER BY",
    );
}

/// Combined GROUP BY + ORDER BY: the sort folds into the grouping b-tree when the
/// ORDER BY is exactly the GROUP BY key (any direction); otherwise both stand.
#[test]
fn group_by_plus_order_by() {
    if !have_sqlite() {
        return;
    }
    // ORDER BY == GROUP BY key → single GROUP BY node (fold).
    check(
        D,
        "SELECT u.x, count(*) FROM u JOIN v ON u.x=v.p GROUP BY u.x ORDER BY u.x",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?) | \
         USE TEMP B-TREE FOR GROUP BY",
    );
    check(
        D,
        "SELECT u.x, count(*) FROM u JOIN v ON u.x=v.p GROUP BY u.x ORDER BY u.x DESC",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?) | \
         USE TEMP B-TREE FOR GROUP BY",
    );
    // ORDER BY the aggregate → grouping node AND a separate sorter.
    check(
        D,
        "SELECT u.x, count(*) c FROM u JOIN v ON u.x=v.p GROUP BY u.x ORDER BY c",
        "SCAN v USING COVERING INDEX iv | SEARCH u USING INTEGER PRIMARY KEY (rowid=?) | \
         USE TEMP B-TREE FOR GROUP BY | USE TEMP B-TREE FOR ORDER BY",
    );
}

/// Negatives: single-table shapes are untouched by the join logic.
#[test]
fn single_table_unchanged() {
    if !have_sqlite() {
        return;
    }
    // A bare single-table DISTINCT/GROUP BY still spills; ORDER BY on the rowid is
    // still elided — the existing single-table paths, unchanged.
    check(
        D,
        "SELECT DISTINCT y FROM u",
        "SCAN u | USE TEMP B-TREE FOR DISTINCT",
    );
    check(D, "SELECT x, count(*) FROM u GROUP BY x", "SCAN u");
    check(D, "SELECT x FROM u ORDER BY x", "SCAN u");
}
