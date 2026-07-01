//! `EXPLAIN QUERY PLAN` over a derived-table / CTE whose body is an *aggregate*
//! (a bare aggregate or a `GROUP BY`) or a `DISTINCT`. Such a body cannot be
//! flattened into the outer plan, so SQLite materializes it as `CO-ROUTINE <label>`
//! whose single child is the body's own plan, followed by the outer
//! `{SCAN|SEARCH} <label>` (plus at most one trailing temp-b-tree) — the same wrapper
//! it uses for a compound body.
//!
//! graphite previously declined any aggregate / DISTINCT body source with `EXPLAIN
//! QUERY PLAN for this query shape`; it now renders the single-base-table case
//! byte-exactly (the body child being the body's already-exact plan) and still
//! declines a join/compound body or a source combined with another table. Verified
//! vs the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// `EXPLAIN QUERY PLAN sql` → `#`-joined bare node labels.
fn plan(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} EXPLAIN QUERY PLAN {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|c: char| " |`*+_-".contains(c)))
        .collect::<Vec<_>>()
        .join("#")
}

const BASE: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); CREATE INDEX tb ON t(b); \
                    CREATE TABLE u(x,y);";

#[test]
fn aggregate_body_coroutine_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM (SELECT count(*) FROM t) AS s",
        "SELECT * FROM (SELECT count(*) AS n FROM t) AS s",
        "SELECT * FROM (SELECT count(*) FROM t WHERE b>0) AS s",
        "SELECT * FROM (SELECT max(b) FROM t) AS s",
        "SELECT * FROM (SELECT b, count(*) FROM t GROUP BY b) AS s",
        "SELECT * FROM (SELECT sum(b),avg(c) FROM t) AS s",
        // A narrower outer projection still reads the single co-routine row.
        "SELECT n FROM (SELECT count(*) AS n FROM t) AS s",
        // An outer ORDER BY over the single aggregate row.
        "SELECT * FROM (SELECT count(*) FROM t) AS s ORDER BY 1",
        // A CTE reference resolves the same way (label = the CTE name).
        "WITH c AS (SELECT count(*) FROM t) SELECT * FROM c",
        // A DISTINCT body materializes the same way — the body child is its own plan
        // (a covering-index scan when the index satisfies DISTINCT, else a temp-btree).
        "SELECT * FROM (SELECT DISTINCT b FROM t) AS s",
        "SELECT * FROM (SELECT DISTINCT a,b FROM t) AS s",
        "SELECT * FROM (SELECT DISTINCT c FROM t) AS s",
        "SELECT x FROM (SELECT DISTINCT b AS x FROM t) AS s",
        "WITH c AS (SELECT DISTINCT b FROM t) SELECT * FROM c",
        // A non-flattenable LIMIT/OFFSET body (B9c): an OFFSET, an outer WHERE, or an
        // outer aggregate takes the same CO-ROUTINE wrapper as an aggregate body.
        "SELECT * FROM (SELECT * FROM t LIMIT 5) AS s WHERE a>3",
        "SELECT * FROM (SELECT * FROM t LIMIT 5 OFFSET 2) AS s",
        "SELECT * FROM (SELECT * FROM t ORDER BY b LIMIT 5 OFFSET 1) AS s",
        "SELECT count(*) FROM (SELECT * FROM t LIMIT 5) AS s",
        "SELECT max(b) FROM (SELECT * FROM t LIMIT 5) AS s",
        "WITH c AS (SELECT * FROM t LIMIT 5) SELECT * FROM c WHERE a>3",
    ] {
        assert_eq!(plan("sqlite3", BASE, q), plan(g, BASE, q), "plan for {q}");
    }
}

#[test]
fn non_renderable_aggregate_bodies_decline() {
    // A join / nested-subquery aggregate body, and an aggregate source combined
    // with another table, fall outside the byte-exact subset and decline cleanly.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM (SELECT count(*) FROM t JOIN u ON t.a=u.x) AS s",
        "SELECT * FROM (SELECT count(*) FROM t) AS s, u",
        "SELECT * FROM (SELECT count(*) FROM (SELECT * FROM t)) AS s",
    ] {
        let full = format!("{BASE} EXPLAIN QUERY PLAN {q}");
        let out = Command::new(g).arg(":memory:").arg(&full).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !err.contains("no such table"),
            "{q} regressed to the malformed crash: {err:?}"
        );
        assert!(
            err.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{q} should decline as unsupported, got stdout {:?} stderr {err:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}
