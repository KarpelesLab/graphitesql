//! Track B (EQP): `GROUP BY` on the rowid / INTEGER PRIMARY KEY degenerates to one
//! row per group, emitted in rowid order. sqlite therefore plain-`SCAN`s the table
//! (it never picks a covering index for it) and, for a sole `ORDER BY` term on that
//! same key, needs no `USE TEMP B-TREE FOR ORDER BY` — both directions. The credit
//! is *not* extended to a multi-term `ORDER BY` (sqlite does not elide trailing
//! terms here via key uniqueness, unlike a plain scan) nor to a non-rowid `GROUP
//! BY`. An aggregate (`count(*)`) and an aggregate `HAVING` are allowed (every group
//! is a single row). Verified byte-exact against sqlite3 — plan and row order.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn g_eqp(c: &Connection, q: &str) -> String {
    c.query(&format!("EXPLAIN QUERY PLAN {q}"))
        .unwrap()
        .rows
        .iter()
        .filter_map(|r| match r.last() {
            Some(Value::Text(s)) => Some(String::from(s.as_str())),
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
                    Value::Text(s) => String::from(s.as_str()),
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
fn group_by_rowid_plain_scans_and_skips_sort() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // Out-of-order explicit rowids so "rowid order" is distinguishable from
    // insertion order; duplicate `a` so a covering-index walk would reorder rows.
    let rows = " INSERT INTO t VALUES (3,5,9),(1,3,7),(2,5,2),(5,1,4),(4,8,6);";

    // (query, expect-no-temp-btree-node). A covering index on `a` exists, so before
    // the fix graphite picked `SCAN t USING COVERING INDEX ia` for the `SELECT id`
    // cases — now it bare-`SCAN`s like sqlite. Every case is byte-exact vs sqlite.
    let cases: &[(&str, bool)] = &[
        // No ORDER BY: bare SCAN, no covering index.
        ("SELECT id FROM t GROUP BY id", true),
        ("SELECT id, count(*) FROM t GROUP BY id", true),
        // Sole ORDER BY term on the rowid/IPK → no sort, both directions.
        ("SELECT id FROM t GROUP BY id ORDER BY id", true),
        ("SELECT id FROM t GROUP BY id ORDER BY id DESC", true),
        ("SELECT id, count(*) FROM t GROUP BY id ORDER BY id", true),
        // `rowid` spelling of the key.
        ("SELECT id FROM t GROUP BY rowid ORDER BY rowid", true),
        // Aggregate HAVING only filters whole singleton groups → still no sort.
        (
            "SELECT id FROM t GROUP BY id HAVING count(*)>0 ORDER BY id",
            true,
        ),
        // Multi-term ORDER BY: sqlite does NOT elide the trailing term here → sorts.
        ("SELECT id FROM t GROUP BY id ORDER BY id, a", false),
        // ORDER BY a non-key column → sorts.
        ("SELECT id FROM t GROUP BY id ORDER BY a", false),
    ];

    for &(q, expect_no_node) in cases {
        let full =
            format!("CREATE TABLE t(id INTEGER PRIMARY KEY,a,b); CREATE INDEX ia ON t(a);{rows}");
        let c = conn(&full);
        let g = g_eqp(&c, q);
        assert_eq!(
            !g.contains("TEMP B-TREE FOR ORDER BY"),
            expect_no_node,
            "node presence wrong for {q}\n  got: {g}"
        );
        // The covering index is never chosen for a rowid GROUP BY.
        assert!(
            !g.contains("COVERING INDEX"),
            "unexpected covering index for {q}\n  got: {g}"
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
fn non_rowid_group_by_is_unchanged() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    let rows = " INSERT INTO t VALUES (3,5,9),(1,3,7),(2,5,2),(5,1,4),(4,8,6);";
    // A GROUP BY on a non-rowid column keeps sqlite's covering-index walk; the rowid
    // recognition must not disturb it. Implicit-rowid table's `GROUP BY rowid` is
    // still credited (bare SCAN, no sort).
    let cases: &[(&str, &str)] = &[
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b); CREATE INDEX ia ON t(a);",
            "SELECT a FROM t GROUP BY a ORDER BY a",
        ),
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b); CREATE INDEX ia ON t(a);",
            "SELECT a, count(*) FROM t GROUP BY a HAVING count(*)>1",
        ),
    ];
    for &(ddl, q) in cases {
        let full = format!("{ddl}{rows}");
        let c = conn(&full);
        let g = g_eqp(&c, q);
        assert_eq!(g, sqlite_eqp(&full, q), "EQP diverged for {q}");
        assert_eq!(
            g_rows(&c, q).trim(),
            sqlite_out(&format!("{full} {q};")).trim(),
            "rows diverged for {q}"
        );
    }

    // Implicit-rowid table (no INTEGER PRIMARY KEY): `GROUP BY rowid` is still the
    // rowid grouping → bare SCAN, no sort.
    let full =
        "CREATE TABLE u(a,b); CREATE INDEX ua ON u(a); INSERT INTO u VALUES(5,9),(3,7),(5,2);";
    let q = "SELECT rowid FROM u GROUP BY rowid ORDER BY rowid";
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
