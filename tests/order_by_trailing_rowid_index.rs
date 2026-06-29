//! Track B (EQP): a secondary index on a rowid table is implicitly ordered by
//! `(key columns…, rowid)`, with the rowid stored ascending. So a query that walks
//! a *non-unique ascending* index for its full key prefix and then orders by the
//! INTEGER PRIMARY KEY (the rowid) needs no sort for that trailing term — and being
//! unique it determines the rest of the order too. `ORDER BY b, id` over an index
//! on `(b)` is served entirely by the walk, with no `USE TEMP B-TREE FOR ORDER BY`,
//! exactly like sqlite. A DESC index column puts the (ascending) rowid out of phase
//! under reversal, so the credit is withheld there (the trailing term is sorted, as
//! before). Verified byte-exact against sqlite3 — plan and row order.

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
fn trailing_rowid_served_by_index_walk() {
    let have_sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    if !have_sqlite {
        return;
    }

    // Dup `b` values so the trailing `id` actually orders within each group.
    let rows = " INSERT INTO t VALUES (3,7,1),(1,7,9),(2,3,5),(4,3,5),(5,3,2);";

    // (ddl-suffix-after-table, query, whether the trailing rowid is served by the
    // walk → no temp b-tree expected). Every case is compared byte-exact to sqlite
    // regardless, so the bool just documents intent.
    let cases: &[(&str, &str, bool)] = &[
        // Non-unique ascending index: trailing id served by the walk.
        (
            "CREATE INDEX ib ON t(b);",
            "SELECT * FROM t ORDER BY b, id",
            true,
        ),
        (
            "CREATE INDEX ib ON t(b);",
            "SELECT * FROM t ORDER BY b DESC, id DESC",
            true,
        ),
        // id moot but c follows: still fully ordered by (b, rowid).
        (
            "CREATE INDEX ib ON t(b);",
            "SELECT * FROM t ORDER BY b, id, c",
            true,
        ),
        // Mixed direction across the b/id boundary: trailing term must be sorted.
        (
            "CREATE INDEX ib ON t(b);",
            "SELECT * FROM t ORDER BY b, id DESC",
            false,
        ),
        // Composite (b,c): the implicit rowid follows c, so b,c,id is fully ordered;
        // b,id is NOT (c sits between b and the rowid in the index).
        (
            "CREATE INDEX ibc ON t(b,c);",
            "SELECT * FROM t ORDER BY b, c, id",
            true,
        ),
        (
            "CREATE INDEX ibc ON t(b,c);",
            "SELECT * FROM t ORDER BY b, id",
            false,
        ),
        // DESC index: the ascending rowid is out of phase → trailing id sorted.
        (
            "CREATE INDEX ibd ON t(b DESC);",
            "SELECT * FROM t ORDER BY b, id",
            false,
        ),
    ];

    for &(idx, q, expect_no_node) in cases {
        let full = format!("CREATE TABLE t(id INTEGER PRIMARY KEY,b,c); {idx}{rows}");
        let c = conn(&full);
        let g = g_eqp(&c, q);
        assert_eq!(
            !g.contains("TEMP B-TREE FOR ORDER BY") && !g.contains("LAST TERM OF ORDER BY"),
            expect_no_node,
            "node presence wrong for [{idx}] {q}\n  got: {g}"
        );
        assert_eq!(g, sqlite_eqp(&full, q), "EQP diverged for [{idx}] {q}");
        // Unique leading prefix or fully-ordered walk → exact ordered comparison.
        assert_eq!(
            g_rows(&c, q).trim(),
            sqlite_out(&format!("{full} {q};")).trim(),
            "rows diverged for [{idx}] {q}"
        );
    }
}

#[test]
fn trailing_rowid_served_by_named_unique_index_walk() {
    let have_sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    if !have_sqlite {
        return;
    }

    // Distinct non-NULL `b`, plus two NULLs (a UNIQUE index permits multiple NULLs;
    // those rows are ordered among themselves by the trailing rowid in the walk).
    let rows = " INSERT INTO t VALUES (3,7,1),(1,NULL,9),(2,3,5),(4,NULL,5),(5,1,2);";

    let cases: &[(&str, &str, bool)] = &[
        // A *named* UNIQUE index has accurate per-column directions, so the trailing
        // rowid credit applies just like a non-unique index — the (b, rowid) walk
        // even orders the multiple NULL-b rows by id.
        (
            "CREATE UNIQUE INDEX ub ON t(b);",
            "SELECT * FROM t ORDER BY b, id",
            true,
        ),
        (
            "CREATE UNIQUE INDEX ub ON t(b);",
            "SELECT * FROM t ORDER BY b DESC, id DESC",
            true,
        ),
        // Mixed direction across the boundary → trailing term sorted.
        (
            "CREATE UNIQUE INDEX ub ON t(b);",
            "SELECT * FROM t ORDER BY b, id DESC",
            false,
        ),
        // A DESC unique index puts the ascending rowid out of phase → not credited.
        (
            "CREATE UNIQUE INDEX ubd ON t(b DESC);",
            "SELECT * FROM t ORDER BY b, id",
            false,
        ),
    ];

    for &(idx, q, expect_no_node) in cases {
        let full = format!("CREATE TABLE t(id INTEGER PRIMARY KEY,b,c); {idx}{rows}");
        let c = conn(&full);
        let g = g_eqp(&c, q);
        assert_eq!(
            !g.contains("TEMP B-TREE FOR ORDER BY") && !g.contains("LAST TERM OF ORDER BY"),
            expect_no_node,
            "node presence wrong for [{idx}] {q}\n  got: {g}"
        );
        assert_eq!(g, sqlite_eqp(&full, q), "EQP diverged for [{idx}] {q}");
        assert_eq!(
            g_rows(&c, q).trim(),
            sqlite_out(&format!("{full} {q};")).trim(),
            "rows diverged for [{idx}] {q}"
        );
    }
}
