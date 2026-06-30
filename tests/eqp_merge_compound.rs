//! `EXPLAIN QUERY PLAN` over a top-level compound (`UNION` / `UNION ALL` /
//! `INTERSECT` / `EXCEPT`) carrying a trailing `ORDER BY`.
//!
//! A trailing `ORDER BY` on the whole compound switches SQLite away from the
//! `COMPOUND QUERY` tree to a `MERGE (<OP>)` plan: each arm is rendered with the
//! `ORDER BY` pushed in (so it streams pre-sorted) and the arms are combined
//! left-associatively under nested `MERGE` nodes — a two-arm compound is a single
//! `MERGE` with `LEFT`/`RIGHT` arm children; three-plus arms nest, the inner
//! `MERGE` becoming the outer's `LEFT` child (each level uses its own operator).
//!
//! Graphite renders this for positional (`ORDER BY 1`, `ORDER BY 2,1`) or bare
//! named/alias (`ORDER BY a`, `ORDER BY b,a`) terms with default null-ordering —
//! a named term is resolved to its result-set position and the whole `ORDER BY`
//! is rewritten to positional before being pushed into the arms. A merge sorts
//! each arm by the *whole output row* whenever a de-duplicating operator
//! (`UNION`/`INTERSECT`/`EXCEPT`) governs that arm (the set operation compares
//! full rows), so a partial cover makes SQLite append the not-yet-covered output
//! columns (ascending) to that arm's sort — surfacing as a per-arm `USE TEMP
//! B-TREE FOR [LAST [N TERMS] OF] ORDER BY`. An arm is governed by a dedup op iff
//! one appears in the operator suffix from that arm onward: in `a UNION b UNION
//! ALL c` the `c` arm (only ever joined by `UNION ALL`) keeps a bare sort while
//! `a`/`b` append; a pure `UNION ALL` compound appends nothing. We reproduce that
//! per-arm. What still declines: an explicit `COLLATE` (SQLite falls to a
//! CO-ROUTINE+materialize shape), an explicit `NULLS FIRST`/`LAST`, a non-column
//! expression term, a `*` projection (column count unresolved), and a `VALUES`
//! arm. Verified vs the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run `EXPLAIN QUERY PLAN sql` and normalise the tree to a `#`-joined line of
/// bare node labels (drop the `QUERY PLAN` header and the box-drawing prefix).
fn plan(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} EXPLAIN QUERY PLAN {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|c: char| " |`*+_-".contains(c)))
        .collect::<Vec<_>>()
        .join("#")
}

/// Raw stdout/stderr for a decline case (graphite emits an `Error:` line).
fn raw(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} EXPLAIN QUERY PLAN {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        return stdout.trim_end().to_string();
    }
    String::from_utf8_lossy(&out.stderr).trim_end().to_string()
}

const BASE: &str =
    "CREATE TABLE t(a,b); CREATE INDEX it ON t(a); CREATE TABLE u(x,y); CREATE INDEX ux ON u(x);";
const DATA: &str = "CREATE TABLE t(a,b); CREATE TABLE u(x,y); \
     INSERT INTO t VALUES(1,2),(3,4),(5,6); INSERT INTO u VALUES(3,9),(5,7),(7,1);";

#[test]
fn compound_order_by_renders_merge() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        // Every operator uses MERGE when a trailing ORDER BY is present — even
        // UNION ALL (which otherwise streams without a sorter).
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY 1",
        "SELECT a FROM t UNION ALL SELECT x FROM u ORDER BY 1",
        "SELECT a FROM t INTERSECT SELECT x FROM u ORDER BY 1",
        "SELECT a FROM t EXCEPT SELECT x FROM u ORDER BY 1",
        // DESC and a whole-compound LIMIT keep the same MERGE shape (no extra node).
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY 1 DESC",
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY 1 LIMIT 5",
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY 1 LIMIT 5 OFFSET 2",
        // A WHERE in an arm renders as that arm's SEARCH inside its MERGE side.
        "SELECT a FROM t WHERE a>0 UNION SELECT x FROM u ORDER BY 1",
        // Multi-column output with the ORDER BY covering all columns (in any
        // order) needs no extra per-arm sort term.
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY 1,2",
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY 2,1",
        // Partial cover: a dedup-governed arm appends the uncovered column(s) as
        // a per-arm `USE TEMP B-TREE FOR LAST TERM OF ORDER BY` (or `... ORDER BY`
        // when even the leading term is not index-satisfiable).
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY 1",
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY 2",
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY 1 DESC",
        "SELECT a,b FROM t INTERSECT SELECT x,y FROM u ORDER BY 1",
        "SELECT a,b FROM t EXCEPT SELECT x,y FROM u ORDER BY 2",
        // A partial cover named/aliased the same way.
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY a",
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY b",
        // UNION ALL with no dedup anywhere appends nothing even on a partial cover.
        "SELECT a,b FROM t UNION ALL SELECT x,y FROM u ORDER BY 1",
        // Mixed ops: only the arms governed by a dedup op (a suffix containing
        // UNION/INTERSECT/EXCEPT) append — the trailing UNION-ALL-only arm does not.
        "SELECT a,b FROM t UNION SELECT x,y FROM u UNION ALL SELECT a,b FROM t ORDER BY 1",
        "SELECT a,b FROM t UNION ALL SELECT x,y FROM u UNION SELECT a,b FROM t ORDER BY 1",
        "SELECT a,b FROM t EXCEPT SELECT x,y FROM u UNION ALL SELECT a,b FROM t ORDER BY 2",
        // A bare named or aliased term resolves to its result-set position (matched
        // case-insensitively) and is rewritten to positional for the arms.
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY a",
        "SELECT a AS p FROM t UNION SELECT x FROM u ORDER BY p",
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY a DESC",
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY b,a",
        "SELECT a,b FROM t INTERSECT SELECT x,y FROM u ORDER BY B,A",
        // Three-plus arms nest left-associatively; the inner MERGE is the outer
        // MERGE's LEFT child, each level carrying its own operator.
        "SELECT a FROM t UNION SELECT x FROM u UNION SELECT a FROM t ORDER BY 1",
        "SELECT a FROM t UNION SELECT x FROM u INTERSECT SELECT a FROM t ORDER BY 1",
        "SELECT a FROM t UNION ALL SELECT x FROM u UNION SELECT a FROM t ORDER BY 1",
        "SELECT a FROM t EXCEPT SELECT x FROM u UNION SELECT a FROM t ORDER BY 1 DESC",
        "SELECT a FROM t UNION SELECT x FROM u UNION SELECT a FROM t ORDER BY a",
    ] {
        assert_eq!(plan("sqlite3", BASE, q), plan(g, BASE, q), "for {q}");
    }
}

#[test]
fn compound_merge_does_not_change_rows() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY 1",
        "SELECT a FROM t UNION ALL SELECT x FROM u ORDER BY 1 DESC",
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY 2,1",
        "SELECT a FROM t INTERSECT SELECT x FROM u ORDER BY 1",
        "SELECT a FROM t UNION SELECT x FROM u UNION SELECT a FROM t ORDER BY 1",
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY a DESC",
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY b,a",
        // Partial cover: each dedup-governed arm appends the uncovered column, but
        // the merged output must still match sqlite row-for-row.
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY 1",
        "SELECT a,b FROM t UNION SELECT x,y FROM u ORDER BY 2 DESC",
        "SELECT a,b FROM t EXCEPT SELECT x,y FROM u UNION ALL SELECT a,b FROM t ORDER BY 1",
    ] {
        let sql = format!("{DATA} {q}");
        let a = Command::new("sqlite3")
            .arg(":memory:")
            .arg(&sql)
            .output()
            .unwrap();
        let b = Command::new(g).arg(":memory:").arg(&sql).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&a.stdout).trim_end(),
            String::from_utf8_lossy(&b.stdout).trim_end(),
            "rows for {q}"
        );
    }
}

#[test]
fn compound_merge_fragile_shapes_decline() {
    // Shapes graphite declines rather than mis-render: an explicit COLLATE (SQLite
    // uses a CO-ROUTINE+materialize plan instead), an explicit NULLS ordering
    // (diverges from our per-arm sort choice), a non-column expression term (no
    // result-set position), and a `*` projection (output column count unresolved
    // here). A partial cover now *renders* (see the render test above).
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY 1 COLLATE NOCASE",
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY a COLLATE NOCASE",
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY 1 NULLS LAST",
        "SELECT a FROM t UNION SELECT x FROM u ORDER BY a+0",
        "SELECT * FROM t UNION SELECT * FROM u ORDER BY 1",
    ] {
        let got = raw(g, BASE, q);
        assert!(
            got.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{q} should decline as unsupported, got {got:?}"
        );
    }
}
