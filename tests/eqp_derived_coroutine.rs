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
    // An *outer* `WHERE` over a pass-through wildcard body also flattens: sqlite
    // pushes the predicate into the scan, so graphite ANDs it into the body and
    // recurses, tightening the `SCAN` into a `SEARCH` (or adding a range bound).
    // A *narrower* outer projection of bare unqualified columns (`SELECT a`,
    // `SELECT a,b`) flattens too: the outer projection is substituted into the body,
    // so it can pick a COVERING-INDEX access path that the full-row body could not.
    // A column qualified by the derived source's own alias (`s.a`) flattens as well —
    // the qualifier names the source itself, so it is stripped on merge and resolves
    // against the base table.
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
        // Outer WHERE pushed into the flattened scan.
        "SELECT * FROM (SELECT * FROM t) AS s WHERE a=5",
        "SELECT * FROM (SELECT * FROM t) AS s WHERE a>5",
        "SELECT * FROM (SELECT a FROM t) AS s WHERE a<5",
        "SELECT * FROM (SELECT * FROM t WHERE a>0) AS s WHERE a<9",
        // Narrower outer projection substituted into the flattened scan; over the
        // index on `a` this becomes a COVERING-INDEX scan/seek.
        "SELECT a FROM (SELECT * FROM t) AS s",
        "SELECT a FROM (SELECT * FROM t) AS s WHERE a<5",
        "SELECT a,b FROM (SELECT * FROM t) WHERE a>0",
        "SELECT a FROM (SELECT a,b FROM t)",
        // Derived-alias-qualified projection/predicate: the `s.` qualifier is
        // stripped on merge, so these flatten the same way as their bare forms.
        "SELECT s.a FROM (SELECT * FROM t) AS s",
        "SELECT s.a, s.b FROM (SELECT * FROM t) AS s WHERE s.a<5",
        "SELECT * FROM (SELECT * FROM t) AS s WHERE s.a=5",
        "SELECT s.a FROM (SELECT * FROM t) AS s WHERE s.a<5",
        // *Aliased* inner projection (`a AS aa`): the derived output name `aa` is
        // mapped back to base column `a` on merge, so the outer reference seeks the
        // index on `a` exactly as a bare projection would.
        "SELECT aa FROM (SELECT a AS aa FROM t) AS s",
        "SELECT aa FROM (SELECT a AS aa FROM t) AS s WHERE aa<5",
        "SELECT * FROM (SELECT a AS aa FROM t) AS s WHERE aa>0",
        "SELECT s.aa FROM (SELECT a AS aa FROM t) AS s WHERE s.aa<5",
    ] {
        let sql = format!("{base} EXPLAIN QUERY PLAN {q}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {q}");
    }
}

#[test]
fn non_flattenable_outer_shapes_decline() {
    // The flatten subset covers a wildcard or narrower outer projection (bare, or
    // qualified by the derived source's own alias) over a single base table whose
    // inner projection is bare columns or `*`, with an optional outer `WHERE` pushed
    // into the scan (see `flattenable_wildcard_over_base_table_matches_sqlite`).
    // Outside it: a *computed* inner projection (`a+1 AS aa`) has no base column to
    // seek on, an outer reference to a column the source does not output
    // (`no such column` in sqlite) cannot resolve, and an inner join/view/LIMIT each
    // change the flattened plan — all decline cleanly rather than mis-render. (An
    // inner *aggregate* or *DISTINCT* body now renders as a CO-ROUTINE — see
    // `tests/eqp_aggregate_coroutine.rs`.)
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a); \
                CREATE TABLE u(x,y); CREATE VIEW v AS SELECT * FROM t;";
    for q in [
        "SELECT * FROM (SELECT a+1 AS aa FROM t) AS s WHERE aa>0", // computed inner projection
        "SELECT b FROM (SELECT a AS aa FROM t) AS s", // outer ref not an output of the source
        "SELECT * FROM (SELECT * FROM t JOIN u ON t.a=u.x) AS s", // inner join
        "SELECT * FROM (SELECT * FROM v) AS s",       // inner view
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
fn derived_table_combined_with_a_join_declines() {
    // A derived-table source combined with another FROM source (a comma-join or an
    // explicit JOIN, in either position) is cost-reordered by sqlite (BLOOM FILTER /
    // AUTOMATIC COVERING INDEX / table reordering) — not byte-exact renderable. It
    // must decline cleanly: a derived *first* source used to crash with a malformed
    // empty `no such table:`, and a derived *join* source used to emit a malformed
    // empty-named `SCAN  AS s`.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a); CREATE TABLE u(x,y);";
    for q in [
        "SELECT * FROM (SELECT * FROM t) AS s, u",
        "SELECT * FROM u, (SELECT * FROM t) AS s",
        "SELECT * FROM (SELECT * FROM t) AS s JOIN u ON s.a=u.x",
        "SELECT * FROM u JOIN (SELECT * FROM t) AS s ON s.a=u.x",
        "SELECT * FROM u LEFT JOIN (SELECT * FROM t) AS s ON s.a=u.x",
        "SELECT * FROM (SELECT 1) AS s, u",
        "SELECT * FROM u, (SELECT 1) AS s",
    ] {
        let sql = format!("{base} EXPLAIN QUERY PLAN {q}");
        let got = run(g, &sql);
        assert!(
            !got.contains("no such table"),
            "{q} regressed to the malformed crash: {got:?}"
        );
        assert!(
            !got.contains("SCAN  AS"),
            "{q} emitted a malformed empty-named node: {got:?}"
        );
        assert!(
            got.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{q} should decline as unsupported, got {got:?}"
        );
    }
}

#[test]
fn cte_reference_renders_like_a_derived_table() {
    // A `WITH`-clause CTE referenced as a FROM source is, to the planner, a derived
    // table whose body is the CTE definition: a constant-row body materializes as
    // `CO-ROUTINE c` (label = the CTE name), and a flattenable base-table body
    // flattens into a plain `SCAN t` (with the inner `WHERE`/`ORDER BY` carried
    // through). graphite previously crashed EQP on any CTE source with
    // `no such table: c`.
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a);";
    for q in [
        "WITH c AS (SELECT * FROM t) SELECT * FROM c",
        "WITH c AS (SELECT 1) SELECT * FROM c",
        "WITH c AS (SELECT 1, 2) SELECT * FROM c",
        "WITH c AS (SELECT * FROM t WHERE a=5) SELECT * FROM c",
        "WITH c AS (SELECT * FROM t ORDER BY b) SELECT * FROM c",
        // Outer WHERE over a flattened CTE pushes into the scan, same as a derived
        // table.
        "WITH c AS (SELECT * FROM t) SELECT * FROM c WHERE a=5",
        "WITH c AS (SELECT a FROM t) SELECT * FROM c WHERE a<5",
        // Narrower outer projection over a flattened CTE → COVERING INDEX, same as
        // a derived table.
        "WITH c AS (SELECT * FROM t) SELECT a FROM c",
        "WITH c AS (SELECT * FROM t) SELECT a FROM c WHERE a<5",
        // CTE-name-qualified projection/predicate: the `c.` qualifier names the CTE
        // itself, so it is stripped on merge and flattens like the bare form.
        "WITH c AS (SELECT * FROM t) SELECT c.a FROM c",
        "WITH c AS (SELECT * FROM t) SELECT * FROM c WHERE c.a=5",
        "WITH c AS (SELECT * FROM t) SELECT c.a FROM c WHERE c.a<5",
        // Aliased CTE body projection (`a AS aa`): the output name maps back to the
        // base column on merge, same as a derived table.
        "WITH c AS (SELECT a AS aa FROM t) SELECT aa FROM c WHERE aa<5",
    ] {
        let sql = format!("{base} EXPLAIN QUERY PLAN {q}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {q}");
    }
}

#[test]
fn non_flattenable_cte_shapes_decline() {
    // The CTE subset mirrors the derived-table one. A join onto the CTE, an inner
    // view, a CTE whose body reads *another* CTE, and an aliased CTE reference each
    // fall outside it and must decline cleanly — never the old `no such table: c`
    // crash. (An *unqualified* or CTE-name-qualified narrower projection and outer
    // WHERE do flatten — see `cte_reference_renders_like_a_derived_table`; an inner
    // aggregate body now renders as a CO-ROUTINE — see
    // `tests/eqp_aggregate_coroutine.rs`.)
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a); \
                CREATE TABLE u(x,y); CREATE VIEW v AS SELECT * FROM t;";
    for q in [
        "WITH c AS (SELECT * FROM t) SELECT * FROM c, u",
        "WITH c AS (SELECT * FROM t), d AS (SELECT * FROM c) SELECT * FROM d",
        "WITH c AS (SELECT * FROM v) SELECT * FROM c",
        "WITH c AS (SELECT * FROM t) SELECT * FROM c AS x",
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
