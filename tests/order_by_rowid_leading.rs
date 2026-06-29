//! Track B (EQP): a single-table full `SCAN` whose `ORDER BY` *leads* with the
//! rowid / INTEGER PRIMARY KEY is already delivered in that order by the scan, so
//! sqlite emits no `USE TEMP B-TREE FOR ORDER BY` node — even when trailing terms
//! follow. Because the leading key is unique, those trailing terms can never break
//! a tie, so the rowid scan order alone fully determines the result. graphite now
//! recognises the multi-term case (previously only a lone `ORDER BY id` / `ORDER BY
//! rowid` was recognised). Verified byte-exact against sqlite3 — both the plan and
//! the (fully determined) row order.

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
fn order_by_leading_rowid_skips_temp_btree() {
    let have_sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    if !have_sqlite {
        return;
    }

    // A rowid table (id is the INTEGER PRIMARY KEY) and a plain table walked via
    // its implicit rowid; some carry a secondary index so we also exercise the
    // covering-index decline path (the scan must stay plain for rowid order).
    let setups: &[&str] = &[
        "CREATE TABLE t(id INTEGER PRIMARY KEY,b,c);",
        "CREATE TABLE t(id INTEGER PRIMARY KEY,b,c); CREATE INDEX ib ON t(b);",
        "CREATE TABLE t(a,b,c);",
    ];
    let data = " INSERT INTO t VALUES (3,7,1),(1,2,9),(2,3,5),(4,1,5);";

    // (query, whether the leading term is the rowid/ipk → no temp b-tree expected)
    let cases: &[(&str, bool)] = &[
        ("SELECT * FROM t ORDER BY id, b", true),
        ("SELECT * FROM t ORDER BY id DESC, b", true),
        ("SELECT * FROM t ORDER BY id ASC, b DESC, c", true),
        ("SELECT * FROM t ORDER BY rowid, b", true),
        ("SELECT * FROM t ORDER BY rowid DESC, c", true),
        ("SELECT id,b FROM t ORDER BY id, b", true),
        // Leading term is NOT the unique key → the sort (and its node) stay.
        ("SELECT * FROM t ORDER BY b, id", false),
        ("SELECT * FROM t ORDER BY c, id DESC", false),
    ];

    for &ddl in setups {
        // The plain-table setup has no `id`/`b`/`c` named `id`; skip id-named queries
        // there and use `rowid` form, which every rowid table understands.
        let has_id = ddl.contains("id INTEGER PRIMARY KEY");
        let full = format!("{ddl}{data}");
        let c = conn(&full);
        for &(q, leads_unique) in cases {
            if !has_id && q.contains(" id") {
                continue;
            }
            // The non-leading cases over a secondary index exercise sqlite's
            // *trailing*-rowid index ordering (`ORDER BY b, id` is fully served by
            // the `(b, rowid)` walk) — a separate, pre-existing gap in graphite's
            // `order_index_scan`, out of scope for this leading-key slice.
            if !leads_unique && ddl.contains("CREATE INDEX") {
                continue;
            }
            let g = g_eqp(&c, q);
            // A leading-unique-key ORDER BY is fully delivered by the scan: no sort
            // node of any kind. (The non-leading cases keep whatever sort node the
            // access path implies — full or partial — which the byte-exact EQP
            // comparison below verifies against sqlite directly.)
            if leads_unique {
                assert!(
                    !g.contains("ORDER BY"),
                    "expected no ORDER BY sort node for [{ddl}] {q}\n  got: {g}"
                );
            }
            assert_eq!(g, sqlite_eqp(&full, q), "EQP diverged for [{ddl}] {q}");
            // The leading unique key fully determines order → exact ordered compare.
            assert_eq!(
                g_rows(&c, q).trim(),
                sqlite_out(&format!("{full} {q};")).trim(),
                "rows diverged for [{ddl}] {q}"
            );
        }
    }
}
