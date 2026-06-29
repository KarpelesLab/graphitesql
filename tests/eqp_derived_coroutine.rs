//! `EXPLAIN QUERY PLAN` over a derived-table (subquery) `FROM` source. SQLite
//! flattens most derived tables into the outer plan (`FROM (SELECT * FROM t)`
//! reads as a plain `SCAN t`), but a *constant-row* body (`FROM (SELECT
//! <consts>)`) can't be flattened — there is no table to merge — so it always
//! materializes as `CO-ROUTINE <label>` whose child is the body's `SCAN CONSTANT
//! ROW`, followed by the outer `SCAN <label>`. graphite renders that byte-exactly
//! when the source is *aliased* (the label is the alias, never the
//! codegen-order-fragile `(subquery-N)` numbering) and the outer query adds no
//! further plan nodes.
//!
//! graphite previously crashed on *any* derived-table source with a malformed
//! `no such table:` (an empty name from looking the subquery up as a b-tree);
//! shapes outside the byte-exact subset now decline cleanly. Verified vs the
//! sqlite3 3.50.4 CLI.

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

/// The byte-exact CO-ROUTINE rendering for an aliased constant-row derived table.
const PLAN: &str = "QUERY PLAN\n|--CO-ROUTINE s\n|  `--SCAN CONSTANT ROW\n`--SCAN s";

#[test]
fn aliased_constant_row_derived_table_is_a_coroutine() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    assert_eq!(
        run(g, "EXPLAIN QUERY PLAN SELECT * FROM (SELECT 1) AS s"),
        PLAN
    );
    // A constant outer WHERE, a LIMIT/OFFSET, an explicit projection, and a
    // multi-column body all keep the same three nodes.
    assert_eq!(
        run(
            g,
            "EXPLAIN QUERY PLAN SELECT * FROM (SELECT 1) AS s WHERE 1"
        ),
        PLAN
    );
    assert_eq!(
        run(
            g,
            "EXPLAIN QUERY PLAN SELECT 1 FROM (SELECT 1) AS s LIMIT 1 OFFSET 0"
        ),
        PLAN
    );
    assert_eq!(
        run(
            g,
            "EXPLAIN QUERY PLAN SELECT x FROM (SELECT 1 AS x, 2 AS y) AS s"
        ),
        PLAN
    );
}

#[test]
fn unrendered_derived_shapes_decline_without_crashing() {
    // The pre-fix bug surfaced as a malformed `no such table:` with an empty name.
    // Shapes outside the byte-exact subset must now decline cleanly instead.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT 1)", // unaliased → fragile numbering
        "EXPLAIN QUERY PLAN SELECT DISTINCT * FROM (SELECT 1) AS s", // +TEMP B-TREE node
        "EXPLAIN QUERY PLAN SELECT *,(SELECT 9) FROM (SELECT 1) AS s", // +SCALAR SUBQUERY
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT * FROM (SELECT 1)) AS s", // body has a FROM
    ] {
        let got = run(g, sql);
        assert!(
            !got.contains("no such table"),
            "{sql} regressed to the malformed crash: {got:?}"
        );
        assert!(
            got.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{sql} should decline as unsupported, got {got:?}"
        );
    }
}

#[test]
fn flattenable_wildcard_over_base_table_matches_sqlite() {
    // A pure `SELECT *` outer over a single *base-table* body is flattened by
    // sqlite into the body's own plan — `FROM (SELECT * FROM t)` reads as a plain
    // `SCAN t`. graphite renders this by recursing into the body under the same
    // parent (its planner produces the identical flattened plan), so an inner
    // `WHERE`/`ORDER BY` carries through (indexed → SEARCH, sort → TEMP B-TREE).
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a);";
    for q in [
        "SELECT * FROM (SELECT * FROM t) AS s",
        "SELECT * FROM (SELECT * FROM t)",
        "SELECT * FROM (SELECT a FROM t) AS s",
        "SELECT * FROM (SELECT * FROM t WHERE a=5) AS s",
        "SELECT * FROM (SELECT * FROM t WHERE a>5) AS s",
        "SELECT * FROM (SELECT * FROM t WHERE b>0) AS s",
        "SELECT * FROM (SELECT * FROM t ORDER BY b) AS s",
        "SELECT * FROM (SELECT * FROM t) AS s LIMIT 5",
    ] {
        let sql = format!("{base} EXPLAIN QUERY PLAN {q}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {q}");
    }
}

#[test]
fn non_flattenable_outer_shapes_decline() {
    // The flatten subset is restricted to a *pure wildcard* outer over a single
    // base table. A narrower outer projection re-derives a covering index, an
    // outer WHERE merges into the scan, and an inner join/aggregate/DISTINCT/view
    // each change the flattened plan — all decline cleanly rather than mis-render.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a); \
                CREATE TABLE u(x,y); CREATE VIEW v AS SELECT * FROM t;";
    for q in [
        "SELECT a FROM (SELECT * FROM t) AS s", // narrower outer projection
        "SELECT * FROM (SELECT * FROM t) AS s WHERE a=5", // outer WHERE
        "SELECT * FROM (SELECT * FROM t JOIN u ON t.a=u.x) AS s", // inner join
        "SELECT * FROM (SELECT DISTINCT a FROM t) AS s", // inner DISTINCT
        "SELECT * FROM (SELECT count(*) FROM t) AS s", // inner aggregate
        "SELECT * FROM (SELECT * FROM v) AS s", // inner view
        "SELECT * FROM (SELECT * FROM t LIMIT 5) AS s", // inner LIMIT
    ] {
        let sql = format!("{base} EXPLAIN QUERY PLAN {q}");
        let got = run(g, &sql);
        assert!(
            !got.contains("no such table"),
            "{q} regressed to the malformed crash: {got:?}"
        );
        assert!(
            got.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{q} should decline as unsupported, got {got:?}"
        );
    }
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT 1) AS s",
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT 1) AS s WHERE 1",
        "EXPLAIN QUERY PLAN SELECT 1 FROM (SELECT 1) AS s LIMIT 1 OFFSET 0",
        "EXPLAIN QUERY PLAN SELECT x FROM (SELECT 1 AS x, 2 AS y) AS s",
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT 'a' || 'b') AS s",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
