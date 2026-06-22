//! Track B (B0b-i remainder): a no-`WHERE` query whose `ORDER BY` shares a
//! uniform leading prefix with a covering index but then changes direction is
//! scanned via that index and only its trailing terms are sorted — reported like
//! sqlite as `USE TEMP B-TREE FOR LAST n TERM[S] OF ORDER BY` (not a full sort),
//! with identical rows. Verified differentially against sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

// Distinct (a,b) prefixes so every tested ORDER BY fully determines row order.
const DDL: &str = "CREATE TABLE t(a,b,c); CREATE INDEX i ON t(a,b,c); \
                   INSERT INTO t VALUES (1,1,1),(1,2,2),(2,1,3),(2,2,4),(3,5,5);";

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

#[test]
fn mixed_direction_partial_sort_over_covering_index() {
    let have_sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let mut c = Connection::open_memory().unwrap();
    for stmt in DDL.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let queries = [
        "SELECT a,b,c FROM t ORDER BY a, b DESC",
        "SELECT a,b,c FROM t ORDER BY a DESC, b",
        "SELECT a,b,c FROM t ORDER BY a,b,c DESC",
        "SELECT a,b,c FROM t ORDER BY a, b DESC, c",
        "SELECT a,b,c FROM t ORDER BY a DESC, b DESC, c",
    ];
    for q in queries {
        let g = g_eqp(&c, q);
        assert!(
            g.contains("USING COVERING INDEX i") && g.contains("LAST"),
            "expected covering index + partial sort, got: {g} ({q})"
        );
        if have_sqlite {
            // sqlite EQP details (strip the tree-drawing prefix).
            let want_eqp = sqlite_out(&format!("{DDL} EXPLAIN QUERY PLAN {q};"))
                .lines()
                .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
                .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).to_string())
                .collect::<Vec<_>>()
                .join(" | ");
            assert_eq!(g, want_eqp, "EQP diverged for: {q}");
            let want_rows = sqlite_out(&format!("{DDL} {q};"));
            assert_eq!(
                g_rows(&c, q).trim(),
                want_rows.trim(),
                "rows diverged for: {q}"
            );
        }
    }
}
