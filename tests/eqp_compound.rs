//! `EXPLAIN QUERY PLAN` over a compound query (`… UNION / UNION ALL / INTERSECT
//! / EXCEPT …`). SQLite renders a `COMPOUND QUERY` node whose first child is the
//! `LEFT-MOST SUBQUERY` (the first arm's plan) followed by one operator node per
//! continuation — `UNION USING TEMP B-TREE`, `UNION ALL`, `INTERSECT USING TEMP
//! B-TREE`, `EXCEPT USING TEMP B-TREE` — each parenting that arm's plan. graphite
//! previously ignored the compound tail entirely and rendered only the first
//! arm's `SCAN`. A trailing `ORDER BY` on the whole compound switches SQLite to a
//! different `MERGE` plan we don't model, so it declines cleanly; a bare
//! `LIMIT`/`OFFSET` keeps the plain tree. Verified vs the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        return stdout.trim_end().to_string();
    }
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .find(|l| !l.trim_start().starts_with('^'))
        .unwrap_or("")
        .trim_start_matches("Error: in prepare, ")
        .trim_start_matches("Error: stepping, ")
        .trim_start_matches("Error: ")
        .trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .trim_end()
        .to_string()
}

const BASE: &str = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a); CREATE TABLE u(x,y);";

#[test]
fn compound_query_tree_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM t UNION SELECT * FROM u",
        "SELECT * FROM t UNION ALL SELECT * FROM u",
        "SELECT a FROM t EXCEPT SELECT x FROM u",
        "SELECT a FROM t INTERSECT SELECT x FROM u",
        "SELECT 1 UNION SELECT 2",
        // A bare LIMIT/OFFSET keeps the plain COMPOUND QUERY tree.
        "SELECT * FROM t UNION SELECT * FROM u LIMIT 5",
        "SELECT * FROM t UNION ALL SELECT * FROM u LIMIT 5 OFFSET 2",
        // Three arms: LEFT-MOST plus two operator nodes.
        "SELECT * FROM t UNION SELECT * FROM u UNION SELECT 9, 9",
        // A per-arm WHERE seek and a shared WITH clause both carry through.
        "SELECT * FROM t WHERE a=5 UNION SELECT * FROM u WHERE x>3",
        "WITH c AS (SELECT 1) SELECT * FROM c UNION SELECT 2",
    ] {
        let sql = format!("{BASE} EXPLAIN QUERY PLAN {q}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {q}");
    }
}

#[test]
fn order_by_on_a_compound_declines() {
    // A trailing ORDER BY makes sqlite emit a `MERGE (UNION)` / `LEFT` / `RIGHT`
    // tree that graphite doesn't model — it must decline cleanly, not mis-render.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM t UNION SELECT * FROM u ORDER BY 1",
        "SELECT a FROM t EXCEPT SELECT x FROM u ORDER BY 1",
    ] {
        let sql = format!("{BASE} EXPLAIN QUERY PLAN {q}");
        let got = run(g, &sql);
        assert!(
            got.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{q} should decline as unsupported, got {got:?}"
        );
    }
}
