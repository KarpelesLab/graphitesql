//! Track B (EQP): `EXPLAIN QUERY PLAN` for a single-table query that groups
//! (`GROUP BY`) or deduplicates (`DISTINCT`) over a plain table `SCAN` emits a
//! `USE TEMP B-TREE FOR GROUP BY` / `FOR DISTINCT` node, exactly like sqlite —
//! placed after the `SCAN` line. SQLite materializes that transient b-tree
//! whenever the bare scan does not already deliver rows clustered by the key
//! columns (which is every case except grouping on the rowid alone). Verified
//! byte-exact against sqlite3, plans and rows.

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

/// sqlite EQP details joined like `g_eqp`, with the tree-drawing prefix stripped.
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
fn group_by_distinct_temp_btree_matches_sqlite() {
    let have_sqlite = Command::new("sqlite3").arg("--version").output().is_ok();

    // (ddl, query, expect_node) — `expect_node` documents whether sqlite (and now
    // graphite) materialize a temp b-tree for this shape.
    let cases: &[(&str, &str, bool)] = &[
        // Plain table, no index: every group/distinct over a bare scan needs the
        // b-tree (rows are not clustered by the key).
        ("CREATE TABLE t(a,b);", "SELECT a FROM t GROUP BY a", true),
        ("CREATE TABLE t(a,b);", "SELECT DISTINCT a FROM t", true),
        ("CREATE TABLE t(a,b);", "SELECT DISTINCT a,b FROM t", true),
        (
            "CREATE TABLE t(a,b);",
            "SELECT a,b FROM t GROUP BY a,b",
            true,
        ),
        // A WHERE filter that does not seek still leaves a bare `SCAN` → node.
        (
            "CREATE TABLE t(a,b);",
            "SELECT a FROM t WHERE b>5 GROUP BY a",
            true,
        ),
        // HAVING does not change the access path → node.
        (
            "CREATE TABLE t(a,b);",
            "SELECT a,count(*) FROM t GROUP BY a HAVING count(*)>=1",
            true,
        ),
        // A secondary index that does NOT lead with the key column is unused for
        // grouping: sqlite plain-scans and adds the node, just like graphite.
        (
            "CREATE TABLE t(a,b); CREATE INDEX ib ON t(b);",
            "SELECT a FROM t GROUP BY a",
            true,
        ),
        (
            "CREATE TABLE t(a,b); CREATE INDEX ib ON t(b);",
            "SELECT DISTINCT a FROM t",
            true,
        ),
        // Grouping / dedup on the rowid alone is already ordered → NO node.
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY,b);",
            "SELECT id FROM t GROUP BY id",
            false,
        ),
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY,b);",
            "SELECT DISTINCT id FROM t",
            false,
        ),
        // A covering index whose leading columns are exactly the keys clusters the
        // rows: covering scan, NO node (graphite already matched here).
        (
            "CREATE TABLE t(a,b); CREATE INDEX it ON t(a,b);",
            "SELECT a FROM t GROUP BY a",
            false,
        ),
        (
            "CREATE TABLE t(a,b); CREATE INDEX it ON t(a,b);",
            "SELECT DISTINCT a,b FROM t",
            false,
        ),
    ];

    for &(ddl, q, expect_node) in cases {
        // The rowid-keyed table needs unique ids; the others exercise real
        // duplicate keys so the grouped/deduped row set actually collapses.
        let rows = if ddl.contains("PRIMARY KEY") {
            "(3,7),(1,2),(2,3),(5,9),(4,1),(6,3)"
        } else {
            "(3,7),(1,2),(2,3),(1,2),(3,9),(2,3)"
        };
        let data = format!("{ddl} INSERT INTO t VALUES {rows};");
        let c = conn(&data);
        let g = g_eqp(&c, q);
        assert_eq!(
            g.contains("TEMP B-TREE"),
            expect_node,
            "node presence wrong for: {q}\n  got: {g}"
        );
        if expect_node {
            let kind = if q.contains("DISTINCT") {
                "USE TEMP B-TREE FOR DISTINCT"
            } else {
                "USE TEMP B-TREE FOR GROUP BY"
            };
            assert!(
                g.ends_with(kind),
                "node should be last for: {q}\n  got: {g}"
            );
        }
        if have_sqlite {
            assert_eq!(g, sqlite_eqp(&data, q), "EQP diverged for: {q}");
            // None of these queries carry an `ORDER BY`, so the row *order* is
            // unspecified (sqlite walks a covering index, graphite the rowid):
            // compare the row *sets*.
            let sorted = |s: &str| {
                let mut v: Vec<String> = s.lines().map(str::to_string).collect();
                v.sort();
                v.join("\n")
            };
            assert_eq!(
                sorted(g_rows(&c, q).trim()),
                sorted(sqlite_out(&format!("{data} {q};")).trim()),
                "row set diverged for: {q}"
            );
        }
    }
}

#[test]
fn temp_btree_node_only_for_bare_scan() {
    // Guard the gate: graphite must NOT bolt a node onto a non-bare scan line.
    // `GROUP BY b` over a covering index on `(a,b)` (graphite picks the covering
    // index, sqlite plain-scans) is a known scan-line divergence we deliberately
    // leave alone — and crucially graphite must not emit a phantom node there.
    let c = conn("CREATE TABLE t(a,b); CREATE INDEX it ON t(a,b);");
    let g = g_eqp(&c, "SELECT b FROM t GROUP BY b");
    assert_eq!(g, "SCAN t USING COVERING INDEX it", "unexpected: {g}");

    // `GROUP BY a, b` over an index on `(a)` only: graphite plain-scans (the index
    // is not covering); we decline the node because the index leads with the first
    // key (sqlite would walk it as `SCAN t USING INDEX it`), so graphite keeps its
    // bare `SCAN t` with no phantom node.
    let c = conn("CREATE TABLE t(a,b); CREATE INDEX it ON t(a);");
    let g = g_eqp(&c, "SELECT a,b FROM t GROUP BY a,b");
    assert_eq!(g, "SCAN t", "unexpected: {g}");
}
