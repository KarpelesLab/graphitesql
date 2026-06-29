//! Track B (EQP): a `DISTINCT` whose projection pins the rowid / INTEGER PRIMARY
//! KEY is a *no-op* — the key is unique per row, so de-duplication removes nothing.
//! sqlite then plans the query exactly as if `DISTINCT` were absent: no `USE TEMP
//! B-TREE FOR DISTINCT`, the table bare-`SCAN`s, and a sole leading `ORDER BY` on
//! that key needs no `USE TEMP B-TREE FOR ORDER BY` (rows already arrive in rowid
//! order). A bare `*` / `t.*` counts (the IPK is in its expansion). An *expression*
//! projection (`id+0`) does NOT — sqlite keeps the DISTINCT b-tree — and is left
//! untouched here. Verified byte-exact against sqlite3 — plan and row order.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn g_eqp(c: &Connection, q: &str) -> String {
    c.query(&format!("EXPLAIN QUERY PLAN {q}"))
        .unwrap()
        .rows
        .iter()
        .filter_map(|r| match r.last() {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn g_rows(c: &Connection, q: &str) -> String {
    c.query(q)
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => format!("{f}"),
                    Value::Text(s) => s.clone(),
                    Value::Null => String::new(),
                    _ => "?".into(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sqlite_out(sql: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn sqlite_eqp(ddl: &str, q: &str) -> String {
    sqlite_out(&format!("{ddl} EXPLAIN QUERY PLAN {q};"))
        .lines()
        .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn conn(ddl: &str) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

#[test]
fn distinct_pinning_rowid_is_noop_and_skips_both_btrees() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // Out-of-order explicit rowids so "rowid order" is distinguishable from
    // insertion order; duplicate `a` so a covering-index walk would reorder rows.
    let rows = " INSERT INTO t VALUES (3,5,9),(1,3,7),(2,5,2),(5,1,4),(4,8,6);";

    // Every case is a no-op DISTINCT (projection pins the IPK `id` or `rowid`), so
    // sqlite emits neither a DISTINCT nor an ORDER BY temp b-tree and bare-`SCAN`s.
    // Before the fix graphite picked a covering index and/or a temp b-tree.
    let queries: &[&str] = &[
        "SELECT DISTINCT id FROM t ORDER BY id",
        "SELECT DISTINCT id FROM t ORDER BY id DESC",
        // A trailing term after the unique key never breaks a tie → still no sort.
        "SELECT DISTINCT id FROM t ORDER BY id, a",
        // `rowid` spelling of the key.
        "SELECT DISTINCT rowid FROM t ORDER BY rowid",
        // Multi-column projections that include the key (either position).
        "SELECT DISTINCT id, a FROM t ORDER BY id",
        "SELECT DISTINCT a, id FROM t ORDER BY id",
        "SELECT DISTINCT b, id FROM t ORDER BY id",
        "SELECT DISTINCT id, a, b FROM t ORDER BY id",
        // `*` / `t.*` expand to include the explicit INTEGER PRIMARY KEY column.
        "SELECT DISTINCT * FROM t ORDER BY id",
        "SELECT DISTINCT t.* FROM t ORDER BY id",
    ];

    for &q in queries {
        let full =
            format!("CREATE TABLE t(id INTEGER PRIMARY KEY,a,b); CREATE INDEX ia ON t(a);{rows}");
        let c = conn(&full);
        let g = g_eqp(&c, q);
        assert!(
            !g.contains("TEMP B-TREE") && !g.contains("COVERING INDEX"),
            "no-op DISTINCT should bare-SCAN with no temp b-tree for {q}\n  got: {g}"
        );
        assert_eq!(g, sqlite_eqp(&full, q), "EQP diverged for {q}");
        assert_eq!(
            g_rows(&c, q).trim(),
            sqlite_out(&format!("{full} {q};")).trim(),
            "rows diverged for {q}"
        );
    }
}

#[test]
fn distinct_implicit_rowid_is_noop() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // No INTEGER PRIMARY KEY column: only an explicit `rowid` reference is the key.
    let full = "CREATE TABLE u(a,b); CREATE INDEX ua ON u(a); \
                INSERT INTO u VALUES(5,9),(3,7),(5,2),(1,4);";
    for q in [
        "SELECT DISTINCT rowid FROM u ORDER BY rowid",
        "SELECT DISTINCT rowid, a FROM u ORDER BY rowid",
    ] {
        let c = conn(full);
        let g = g_eqp(&c, q);
        assert!(
            !g.contains("TEMP B-TREE") && !g.contains("COVERING INDEX"),
            "got: {g}"
        );
        assert_eq!(g, sqlite_eqp(full, q), "EQP diverged for {q}");
        assert_eq!(
            g_rows(&c, q).trim(),
            sqlite_out(&format!("{full} {q};")).trim(),
            "rows diverged for {q}"
        );
    }
}

#[test]
fn distinct_without_pinning_rowid_is_unchanged() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    let rows = " INSERT INTO t VALUES (3,5,9),(1,3,7),(2,5,2),(5,1,4),(4,8,6);";
    // A genuine DISTINCT (no unique key in the projection) keeps sqlite's DISTINCT
    // b-tree; the no-op recognition must not touch it. Stays byte-exact vs sqlite.
    let queries: &[&str] = &["SELECT DISTINCT a FROM t ORDER BY a"];
    for &q in queries {
        let full =
            format!("CREATE TABLE t(id INTEGER PRIMARY KEY,a,b); CREATE INDEX ia ON t(a);{rows}");
        let c = conn(&full);
        let g = g_eqp(&c, q);
        assert_eq!(g, sqlite_eqp(&full, q), "EQP diverged for {q}");
        assert_eq!(
            g_rows(&c, q).trim(),
            sqlite_out(&format!("{full} {q};")).trim(),
            "rows diverged for {q}"
        );
    }
}
