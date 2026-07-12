//! Track B (EQP): a `WITHOUT ROWID` table is a b-tree clustered by its PRIMARY
//! KEY, so a PK seek (an equality on a leading-key prefix, or a range on the
//! leading key) walks the matching rows in PK-clustered `storage_order` — PK
//! columns first, then the payload — ascending. The executor tries a PK
//! seek/range *before* any secondary index, so those rows arrive in PK order.
//! When, after dropping the equality-pinned (constant) columns, the `ORDER BY`
//! is a uniform-direction prefix of the remaining PK walk, the seek already
//! produces the requested order — no sorter — exactly as SQLite plans it
//! (`SEARCH w USING PRIMARY KEY …`, no `USE TEMP B-TREE FOR ORDER BY`).
//! graphite previously declined every `WITHOUT ROWID` seek's ordered output and
//! spuriously emitted the sorter.
//!
//! Covered here, byte-exact vs sqlite3 3.50.4 (plan and rows): an equality on a
//! single-column PK serving an `ORDER BY` on the payload; an equality on a
//! leading composite-PK prefix (`x=1 ORDER BY y`, the `DESC` walk, `ORDER BY y,
//! z` continuing into the payload, and `ORDER BY x, y` where the pinned `x` is
//! dropped); a full-key equality with a payload `ORDER BY`; and a leading-key
//! range (`x>1 ORDER BY x, y`, `x>=1 AND x<2 ORDER BY x, y, z`).
//!
//! Deliberately still declined (verified *unchanged* / not wrongly elided):
//!  * a `DESC` PRIMARY KEY range — graphite stores the PK ascending regardless,
//!    so eliding would diverge from sqlite's DESC-clustered storage.
//!  * a seek served by a *secondary* index (`WHERE y=2` with an index on `y`),
//!    whose walk is the index's order, not the PK's.
//!  * a non-prefix / mixed-direction `ORDER BY` that sqlite serves with a
//!    *partial* "LAST TERM" sorter — a separate, pre-existing divergence.

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

/// A PK seek (equality on a leading-key prefix, or a leading-key range) yields
/// rows in PK-clustered order; the post-seek walk serves a uniform-direction
/// `ORDER BY` prefix (equality-pinned columns dropped) — no sorter.
#[test]
fn without_rowid_seek_serves_order() {
    if !have_sqlite() {
        return;
    }
    // A leading-prefix equality pins `x`; the seek then walks `y` (then `z`) in
    // order — no `USE TEMP B-TREE FOR ORDER BY`.
    assert_eq!(
        g_eqp(COMPOSITE, "SELECT * FROM w WHERE x=1 ORDER BY y"),
        "SEARCH w USING PRIMARY KEY (x=?)"
    );
    for (ddl, q) in [
        // Single-column PK: equality pins `a`, payload `b` rides the seek.
        (SINGLE, "SELECT * FROM w WHERE a='a' ORDER BY b"),
        (SINGLE, "SELECT * FROM w WHERE a>'a' ORDER BY a"),
        (SINGLE, "SELECT * FROM w WHERE a>'a' ORDER BY a DESC"),
        // Composite PK, leading-prefix equality.
        (COMPOSITE, "SELECT * FROM w WHERE x=1 ORDER BY y"),
        (COMPOSITE, "SELECT * FROM w WHERE x=1 ORDER BY y DESC"),
        (COMPOSITE, "SELECT * FROM w WHERE x=1 ORDER BY y, z"),
        // The pinned `x` is constant, so a leading `ORDER BY x` is dropped.
        (COMPOSITE, "SELECT * FROM w WHERE x=1 ORDER BY x, y"),
        // Full-key equality; payload `z` rides the seek.
        (COMPOSITE, "SELECT * FROM w WHERE x=1 AND y=2 ORDER BY z"),
        (
            COMPOSITE,
            "SELECT * FROM w WHERE x=1 AND y=2 ORDER BY x, y, z",
        ),
        // Leading-key range; the walk stays in PK order.
        (COMPOSITE, "SELECT * FROM w WHERE x>1 ORDER BY x, y"),
        (
            COMPOSITE,
            "SELECT * FROM w WHERE x>=1 AND x<2 ORDER BY x, y, z",
        ),
    ] {
        let plan = g_eqp(ddl, q);
        assert!(
            !plan.contains("ORDER BY"),
            "expected no ORDER BY sorter for {q}, got {plan}"
        );
        check(ddl, q);
    }
}

/// A seek served by a *secondary* index is not elided: the walk is not the PK's
/// clustered order this slice models, so the sorter is kept (rows still right).
#[test]
fn without_rowid_seek_declines_non_pk_order() {
    if !have_sqlite() {
        return;
    }
    // Secondary index on `y`: `WHERE y=2` seeks `iy`, walking `y`'s order, not
    // the PK's — graphite keeps the sorter for `ORDER BY x`.
    let sec = "CREATE TABLE w(x,y,z, PRIMARY KEY(x,y)) WITHOUT ROWID; \
        CREATE INDEX iy ON w(y); INSERT INTO w VALUES(2,1,9),(1,2,8),(3,2,7);";
    assert!(g_eqp(sec, "SELECT * FROM w WHERE y=2 ORDER BY x").contains("ORDER BY"));
    assert_eq!(
        g_rows(sec, "SELECT * FROM w WHERE y=2 ORDER BY x"),
        sqlite_rows(sec, "SELECT * FROM w WHERE y=2 ORDER BY x")
    );
}

/// A `DESC` PRIMARY KEY now orders the clustered b-tree descending (byte-compat
/// with sqlite), so the PK-clustered walk yields `a` descending and the sorter is
/// elided in *both* directions — matching sqlite's plan and rows exactly.
#[test]
fn without_rowid_desc_pk_order_elided() {
    if !have_sqlite() {
        return;
    }
    let desc = "CREATE TABLE w(a,b, PRIMARY KEY(a DESC)) WITHOUT ROWID; \
        INSERT INTO w VALUES(3,1),(1,2),(2,3),(5,7),(4,8);";
    for q in [
        "SELECT * FROM w WHERE a>1 ORDER BY a",
        "SELECT * FROM w WHERE a>1 ORDER BY a DESC",
        "SELECT a FROM w ORDER BY a",
        "SELECT a FROM w ORDER BY a DESC",
    ] {
        assert!(
            !g_eqp(desc, q).contains("ORDER BY"),
            "expected no sorter for {q}, got {}",
            g_eqp(desc, q)
        );
        assert_eq!(g_rows(desc, q), sqlite_rows(desc, q), "rows differ for {q}");
    }
}
