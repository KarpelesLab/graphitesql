//! Track B (EQP): a `WITHOUT ROWID` table whose `WHERE` constrains only
//! non-seekable columns is still walked by a full scan of the PK-clustered
//! b-tree, so the surviving rows arrive in PK storage order — SQLite plans a
//! bare `SCAN w` and elides the sorter when the (constant-column-dropped)
//! `ORDER BY` is a uniform-direction contiguous prefix of that order. graphite
//! kept a spurious `USE TEMP B-TREE FOR ORDER BY`.
//!
//! This is the full-scan companion to the PK-seek slice: the seek slice owns a
//! leading-PK equality/range, and `order_index_scan` never picks a secondary
//! index for *ordering* on a `WITHOUT ROWID` table, so the PK-ordered walk holds
//! whenever neither the leading PK column nor any secondary index's leading
//! column is constrained (which would steer the executor onto a seek instead).
//!
//! Covered here, byte-exact vs sqlite3 3.50.4 (plan and rows): a non-PK range /
//! equality filter with an `ORDER BY` on a single-column PK and on a composite-PK
//! prefix (the full key, the key plus payload, and the `DESC` walk); a pinned
//! non-leading PK column dropped from the `ORDER BY`; and a secondary index whose
//! leading column is *unconstrained* (so the scan still rides the PK b-tree).
//!
//! Deliberately still declined (verified unchanged / not wrongly elided):
//!  * a constrained secondary-index leading column (the executor seeks the index,
//!    walking its order, not the PK's) — graphite keeps its sorter.
//!  * an *internal* pinned-column skip (`WHERE y=2 ORDER BY x, z`): the later term
//!    is functionally determined, but SQLite keeps a *partial* "LAST TERM" sorter
//!    graphite does not model, so this stays the pre-existing full-sorter
//!    divergence rather than becoming a new one.

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
            lines.push(String::from(s.as_str()));
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
                        let s = format!("{f}");
                        if s.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                            format!("{s}.0")
                        } else {
                            s
                        }
                    }
                    graphitesql::Value::Text(s) => String::from(s.as_str()),
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

const SINGLE: &str = "CREATE TABLE w(a TEXT PRIMARY KEY, b) WITHOUT ROWID; \
    INSERT INTO w VALUES('c',3),('a',1),('b',2);";
const COMPOSITE: &str = "CREATE TABLE w(x,y,z, PRIMARY KEY(x,y)) WITHOUT ROWID; \
    INSERT INTO w VALUES(2,1,9),(1,2,8),(1,1,7),(2,2,6),(1,3,5);";
// PK(x,y) with two payload columns and a secondary index on `q`.
const INDEXED: &str = "CREATE TABLE w(x,y,p,q, PRIMARY KEY(x,y)) WITHOUT ROWID; \
    CREATE INDEX iq ON w(q); INSERT INTO w VALUES(2,1,9,1),(1,2,8,2),(1,1,7,3),(2,2,6,4),(1,3,5,5);";

/// A `WHERE` filtering only non-seekable columns leaves the full PK-clustered
/// scan in place, so a uniform `ORDER BY` prefix of the storage order needs no
/// sorter — a bare `SCAN w`, exactly like SQLite.
#[test]
fn without_rowid_filtered_scan_serves_order() {
    if !have_sqlite() {
        return;
    }
    assert_eq!(
        g_eqp(COMPOSITE, "SELECT * FROM w WHERE z>5 ORDER BY x, y"),
        "SCAN w"
    );
    for (ddl, q) in [
        // Single-column PK: a payload filter, payload `ORDER BY` on the PK.
        (SINGLE, "SELECT * FROM w WHERE b>1 ORDER BY a"),
        // Composite PK: range / equality on the payload, ORDER BY the PK prefix,
        // the full key, the key plus payload, and the DESC walk.
        (COMPOSITE, "SELECT * FROM w WHERE z>5 ORDER BY x, y"),
        (COMPOSITE, "SELECT * FROM w WHERE z>5 ORDER BY x, y, z"),
        (COMPOSITE, "SELECT * FROM w WHERE z=9 ORDER BY x, y"),
        (
            COMPOSITE,
            "SELECT * FROM w WHERE z>5 ORDER BY x DESC, y DESC",
        ),
        // A pinned *non-leading* PK column is constant, so it drops out of the
        // ORDER BY and the leading column still rides the scan.
        (COMPOSITE, "SELECT * FROM w WHERE y=2 ORDER BY x"),
        // A secondary index whose leading column is unconstrained is not seeked,
        // so the scan still walks the PK b-tree.
        (INDEXED, "SELECT * FROM w WHERE p>5 ORDER BY x, y"),
        (INDEXED, "SELECT * FROM w WHERE p=8 ORDER BY x, y"),
    ] {
        let plan = g_eqp(ddl, q);
        assert!(
            !plan.contains("ORDER BY"),
            "expected no ORDER BY sorter for {q}, got {plan}"
        );
        check(ddl, q);
    }
}

/// A constrained secondary-index leading column steers the executor onto an index
/// seek (walking the index's order, not the PK's), and an internal pinned-column
/// skip is SQLite's partial-sorter case — both keep graphite's sorter, with the
/// rows still correct.
#[test]
fn without_rowid_filtered_scan_declines() {
    if !have_sqlite() {
        return;
    }
    // The index's leading column `q` is constrained — graphite seeks the index
    // and keeps the sorter (the index-vs-scan choice is a separate, pre-existing
    // divergence; the point here is graphite does not wrongly elide).
    assert!(g_eqp(INDEXED, "SELECT * FROM w WHERE q>2 ORDER BY x, y").contains("ORDER BY"));
    assert_eq!(
        g_rows(INDEXED, "SELECT * FROM w WHERE q>2 ORDER BY x, y"),
        sqlite_rows(INDEXED, "SELECT * FROM w WHERE q>2 ORDER BY x, y")
    );
    // An internal pinned-column skip: `y` (pinned) sits between `x` and `z` in
    // the ORDER BY, so SQLite keeps a partial "LAST TERM" sorter graphite does
    // not model — graphite keeps its full sorter, rows still correct.
    assert!(g_eqp(COMPOSITE, "SELECT * FROM w WHERE y=2 ORDER BY x, z").contains("ORDER BY"));
    assert_eq!(
        g_rows(COMPOSITE, "SELECT * FROM w WHERE y=2 ORDER BY x, z"),
        sqlite_rows(COMPOSITE, "SELECT * FROM w WHERE y=2 ORDER BY x, z")
    );
}
