//! An explicit `NULLS FIRST`/`LAST` whose placement matches what a uniform index
//! or PK walk already produces — `ASC NULLS FIRST` / `DESC NULLS LAST` — is
//! *redundant*: it orders identically to a bare term, so SQLite serves it from the
//! index with no sorter (no `USE TEMP B-TREE FOR ORDER BY`). graphite previously
//! bailed on any explicit `NULLS` clause and always added the sorter; it now treats
//! the redundant clause exactly like a bare term across the single-table scan, the
//! covering scan, the `WHERE`-seek prefix, and a compound `MERGE` arm.
//!
//! The *opposite* placement — `ASC NULLS LAST` / `DESC NULLS FIRST` — is NOT what a
//! single walk yields; SQLite serves it with a two-pass index scan graphite does
//! not model, so graphite keeps its sorter there. That divergence is a known,
//! documented residual and is intentionally not asserted against SQLite here.
//! Verified vs the sqlite3 3.50.4 CLI.

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

/// Executed rows, `|`-joined.
fn rows(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

#[test]
fn redundant_nulls_is_index_satisfied_like_a_bare_term() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // One index per schema so the index choice is unambiguous (an overlapping
    // `(a)` + `(a,b)` pair is a separate, pre-existing selection divergence).
    let single = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a);";
    let multi = "CREATE TABLE t(a,b); CREATE INDEX itab ON t(a,b);";
    let compound = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a); \
                    CREATE TABLE u(x,y); CREATE INDEX ux ON u(x);";
    for (base, q) in [
        // Single-column covering scan, both natural directions.
        (single, "SELECT a FROM t ORDER BY a ASC NULLS FIRST"),
        (single, "SELECT a FROM t ORDER BY a DESC NULLS LAST"),
        // Non-covering (SELECT a,b over an index on a) — same.
        (single, "SELECT a,b FROM t ORDER BY a NULLS FIRST"),
        (single, "SELECT a,b FROM t ORDER BY a DESC NULLS LAST"),
        // Multi-column index, every term redundant.
        (
            multi,
            "SELECT a,b FROM t ORDER BY a NULLS FIRST, b NULLS FIRST",
        ),
        (
            multi,
            "SELECT a,b FROM t ORDER BY a DESC NULLS LAST, b DESC NULLS LAST",
        ),
        // A `WHERE` range/equality seek whose walk yields the order.
        (multi, "SELECT a,b FROM t WHERE a>0 ORDER BY a NULLS FIRST"),
        (multi, "SELECT a,b FROM t WHERE a=1 ORDER BY b NULLS FIRST"),
        // Compound MERGE arms (full and partial cover) carry the redundant clause.
        (
            compound,
            "SELECT a FROM t UNION SELECT x FROM u ORDER BY 1 NULLS FIRST",
        ),
        (
            compound,
            "SELECT a FROM t UNION SELECT x FROM u ORDER BY a NULLS FIRST",
        ),
        (
            compound,
            "SELECT a FROM t UNION SELECT x FROM u ORDER BY 1 DESC NULLS LAST",
        ),
    ] {
        assert_eq!(plan("sqlite3", base, q), plan(g, base, q), "plan for {q}");
        // And the redundant clause must order rows exactly like the bare term.
        let bare = q
            .replace(" ASC NULLS FIRST", "")
            .replace(" DESC NULLS LAST", " DESC")
            .replace(" NULLS FIRST", "");
        let data = "INSERT INTO t VALUES(3,1),(NULL,2),(1,3),(NULL,NULL),(2,5); \
                    CREATE TABLE IF NOT EXISTS u(x,y); INSERT INTO u VALUES(5,9),(NULL,8),(2,7);";
        let with = format!("{base} {data}");
        assert_eq!(rows(g, &with, q), rows("sqlite3", &with, q), "rows for {q}");
        assert_eq!(
            rows(g, &with, q),
            rows(g, &with, &bare),
            "redundant ≡ bare for {q}"
        );
    }
}

#[test]
fn opposite_nulls_placement_keeps_the_sorter_but_orders_correctly() {
    // The two-pass cases graphite does not model: it keeps a sorter (an EQP
    // divergence from sqlite's index two-pass) but the executed rows are still
    // correct, which is what we assert here.
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "CREATE TABLE t(a,b); CREATE INDEX it ON t(a); \
                INSERT INTO t VALUES(3,1),(NULL,2),(1,3),(NULL,4),(2,5);";
    for q in [
        "SELECT a FROM t ORDER BY a ASC NULLS LAST",
        "SELECT a FROM t ORDER BY a DESC NULLS FIRST",
    ] {
        assert_eq!(rows(g, base, q), rows("sqlite3", base, q), "rows for {q}");
    }
}
