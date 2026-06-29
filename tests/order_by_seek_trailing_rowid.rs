//! Track B (EQP): the trailing-rowid credit, on the `WHERE`-seek path. When a seek
//! walks a secondary index for its whole key (after any equality-pinned prefix), the
//! walk continues in rowid order, so an `ORDER BY` that ends in the INTEGER PRIMARY
//! KEY (the rowid) needs no sort for that term — `WHERE b>2 ORDER BY b, id` over an
//! index on `(b)`, or `WHERE b=3 ORDER BY id`, is served entirely by the seek, like
//! sqlite (no `USE TEMP B-TREE FOR ORDER BY`). The credit is withheld for a DESC
//! index column (the ascending rowid falls out of phase under reversal), a mixed
//! direction boundary, and an automatic UNIQUE/PK index (directions unknown).
//! Verified byte-exact against sqlite3 — plan and row order.

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
fn seek_trailing_rowid_served_by_index_walk() {
    let have_sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    if !have_sqlite {
        return;
    }

    // Dup `b` so the trailing `id` actually orders within each group.
    let rows = " INSERT INTO t VALUES (3,7,1),(1,7,9),(2,3,5),(4,3,5),(5,3,2);";

    // (ddl-suffix, query, expect-no-sort-node). Every case is byte-exact vs sqlite.
    let cases: &[(&str, &str, bool)] = &[
        // Range seek + trailing rowid.
        (
            "CREATE INDEX ib ON t(b);",
            "SELECT * FROM t WHERE b>2 ORDER BY b, id",
            true,
        ),
        (
            "CREATE INDEX ib ON t(b);",
            "SELECT * FROM t WHERE b>2 ORDER BY b DESC, id DESC",
            true,
        ),
        // Mixed direction across the boundary → trailing term sorted.
        (
            "CREATE INDEX ib ON t(b);",
            "SELECT * FROM t WHERE b>2 ORDER BY b, id DESC",
            false,
        ),
        // Equality seek: after `b=?` the walk is pure rowid order.
        (
            "CREATE INDEX ib ON t(b);",
            "SELECT * FROM t WHERE b=3 ORDER BY id",
            true,
        ),
        (
            "CREATE INDEX ib ON t(b);",
            "SELECT * FROM t WHERE b=3 ORDER BY id DESC",
            true,
        ),
        // Composite with an equality prefix: `b=? ORDER BY c, id` walks (c, rowid).
        (
            "CREATE INDEX ibc ON t(b,c);",
            "SELECT * FROM t WHERE b=7 ORDER BY c, id",
            true,
        ),
        // Composite, trailing term is NOT the next walked column → sorted.
        (
            "CREATE INDEX ibc ON t(b,c);",
            "SELECT * FROM t WHERE b=7 ORDER BY id",
            false,
        ),
        // A DESC index column puts the ascending rowid out of phase → sorted.
        (
            "CREATE INDEX ibd ON t(b DESC);",
            "SELECT * FROM t WHERE b>2 ORDER BY b, id",
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

#[test]
fn seek_trailing_rowid_named_unique_index() {
    let have_sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    if !have_sqlite {
        return;
    }

    // Distinct non-NULL `b` plus two NULLs (multiple NULLs allowed; ordered by rowid).
    let rows = " INSERT INTO t VALUES (3,7,1),(1,NULL,9),(2,3,5),(4,NULL,5),(5,1,2);";
    let cases: &[(&str, &str, bool)] = &[
        (
            "CREATE UNIQUE INDEX ub ON t(b);",
            "SELECT * FROM t WHERE b>2 ORDER BY b, id",
            true,
        ),
        (
            "CREATE UNIQUE INDEX ubd ON t(b DESC);",
            "SELECT * FROM t WHERE b>2 ORDER BY b, id",
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
