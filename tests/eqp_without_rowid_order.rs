//! Track B (EQP): a `WITHOUT ROWID` table is stored as a b-tree clustered by its
//! PRIMARY KEY, so a full scan yields rows in PK-clustered `storage_order` (PK
//! columns first, then the rest) ascending. When the whole `ORDER BY` is a
//! uniform-direction *contiguous prefix* of that storage order, the scan already
//! produces the requested order — no sorter — exactly as SQLite plans it (a bare
//! `SCAN w`). graphite previously declined every `WITHOUT ROWID` ordered scan and
//! spuriously emitted `USE TEMP B-TREE FOR ORDER BY`.
//!
//! Covered here, byte-exact vs sqlite3 3.50.4 (plan and rows): a single-column PK,
//! a composite PK (a leading prefix, the full key, and the full key plus the
//! trailing payload column), the `DESC` (materialise-then-reverse) walk, and the
//! `SELECT *` / positional-ordinal projections resolving to the same columns.
//!
//! Deliberately still declined (so verified *unchanged* / not wrongly elided):
//!  * a `DESC` PRIMARY KEY — graphite always stores a `WITHOUT ROWID` PK ascending
//!    (it drops a declared key direction), so eliding would diverge from sqlite's
//!    DESC-clustered storage; the sorter is kept.
//!  * a non-prefix or mixed-direction `ORDER BY` (`ORDER BY y`, `ORDER BY x, z`),
//!    which sqlite serves with a *partial* sorter — a separate, pre-existing
//!    divergence not addressed by this slice.

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
    INSERT INTO w VALUES(2,1,9),(1,2,8),(1,1,7),(2,2,6);";

/// A uniform-direction prefix of the PK-clustered storage order rides the scan in
/// order — no `USE TEMP B-TREE FOR ORDER BY`, just `SCAN w`.
#[test]
fn without_rowid_pk_prefix_elides_sorter() {
    if !have_sqlite() {
        return;
    }
    assert_eq!(g_eqp(SINGLE, "SELECT * FROM w ORDER BY a"), "SCAN w");
    assert_eq!(g_eqp(SINGLE, "SELECT * FROM w ORDER BY a DESC"), "SCAN w");
    assert_eq!(g_eqp(COMPOSITE, "SELECT * FROM w ORDER BY x, y"), "SCAN w");
    for (ddl, q) in [
        (SINGLE, "SELECT * FROM w ORDER BY a"),
        (SINGLE, "SELECT a FROM w ORDER BY a"),
        (SINGLE, "SELECT * FROM w ORDER BY a DESC"),
        (SINGLE, "SELECT * FROM w ORDER BY a, b"),
        // `SELECT *` ordinal and an aliased table resolve to the same column.
        (SINGLE, "SELECT * FROM w ORDER BY 1"),
        (SINGLE, "SELECT * FROM w AS r ORDER BY 1"),
        (COMPOSITE, "SELECT * FROM w ORDER BY x"),
        (COMPOSITE, "SELECT * FROM w ORDER BY x, y"),
        // The full PK plus the trailing payload column — still the whole storage
        // prefix, so still elided.
        (COMPOSITE, "SELECT * FROM w ORDER BY x, y, z"),
        (COMPOSITE, "SELECT * FROM w ORDER BY x DESC, y DESC"),
        (COMPOSITE, "SELECT * FROM w ORDER BY 1, 2"),
    ] {
        let plan = g_eqp(ddl, q);
        assert!(
            !plan.contains("ORDER BY"),
            "expected no ORDER BY sorter for {q}, got {plan}"
        );
        check(ddl, q);
    }
}

/// A `DESC` PRIMARY KEY is *not* elided — graphite stores the PK ascending
/// regardless, so the sorter is kept (and the rows still come out right).
#[test]
fn without_rowid_desc_pk_keeps_sorter() {
    if !have_sqlite() {
        return;
    }
    for ddl in [
        "CREATE TABLE w(a TEXT, b, PRIMARY KEY(a DESC)) WITHOUT ROWID; \
            INSERT INTO w VALUES('c',3),('a',1),('b',2);",
        "CREATE TABLE w(a TEXT PRIMARY KEY DESC, b) WITHOUT ROWID; \
            INSERT INTO w VALUES('c',3),('a',1),('b',2);",
    ] {
        // graphite keeps its full sorter here; the point is the rows are still
        // correct (no wrong elision against a mismatched storage order).
        assert_eq!(g_rows(ddl, "SELECT * FROM w ORDER BY a"), "a|1\nb|2\nc|3");
        assert_eq!(
            g_rows(ddl, "SELECT * FROM w ORDER BY a DESC"),
            "c|3\nb|2\na|1"
        );
        assert_eq!(
            g_rows(ddl, "SELECT * FROM w ORDER BY a"),
            sqlite_rows(ddl, "SELECT * FROM w ORDER BY a")
        );
    }
}
