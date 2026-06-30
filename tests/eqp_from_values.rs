//! `EXPLAIN QUERY PLAN` over a multi-row `VALUES (…),(…),…` clause used as a
//! derived `FROM` source. SQLite reads such a clause directly — no `CO-ROUTINE`
//! wrapper — as a single `SCAN N-ROW VALUES CLAUSE` node (or `SEARCH …` when a
//! lone `min`/`max` triggers the no-op-scan optimization), plus at most one
//! trailing `USE TEMP B-TREE FOR ORDER BY|GROUP BY|DISTINCT` for the outer
//! query. A *single-row* `VALUES(…)` source is still wrapped in a co-routine
//! (`CO-ROUTINE (subquery-1)#SCAN CONSTANT ROW#SCAN (subquery-1)`) which we don't
//! model from this path, so it declines; a row carrying a subquery, a multi-table
//! FROM/JOIN, or a combination of outer ORDER BY/GROUP BY/DISTINCT clauses all
//! switch SQLite to shapes we don't model and likewise decline cleanly rather
//! than mis-render. Verified vs the sqlite3 3.50.4 CLI.

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

#[test]
fn from_values_renders_single_node() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        // Bare multi-row VALUES source folds to one SCAN N-ROW VALUES CLAUSE.
        "SELECT * FROM (VALUES(1,2),(3,4))",
        "SELECT * FROM (VALUES(1,2),(3,4)) t",
        "SELECT column1 FROM (VALUES(1,2),(3,4)) WHERE column2>2",
        "SELECT * FROM (VALUES(1,2),(3,4)) LIMIT 1",
        // A single outer ORDER BY / GROUP BY / DISTINCT appends one temp-b-tree.
        "SELECT * FROM (VALUES(1,2),(3,4)) ORDER BY column1 DESC",
        "SELECT column1 FROM (VALUES(1,2),(3,4)) GROUP BY column1",
        "SELECT column1 FROM (VALUES(1,2),(3,4)) GROUP BY column1 HAVING column1>1",
        "SELECT DISTINCT column1 FROM (VALUES(1),(2),(2))",
        // A lone min/max over the clause uses the SEARCH optimization.
        "SELECT max(column1) FROM (VALUES(1),(2),(3))",
        "SELECT min(column1) FROM (VALUES(3),(1),(2))",
        // count(*) and grouped aggregates keep the SCAN.
        "SELECT count(*) FROM (VALUES(1),(2))",
        "SELECT column1, count(*) FROM (VALUES(1),(1),(2)) GROUP BY column1",
    ] {
        let sql = format!("EXPLAIN QUERY PLAN {q}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {q}");
        // The executed rows must agree too.
        assert_eq!(run("sqlite3", q), run(g, q), "rows for {q}");
    }
}

#[test]
fn from_values_edge_cases_decline() {
    // A single-row VALUES source (co-routine wrapper), a subquery inside a row, a
    // multi-table FROM, and a combination of outer clauses each switch SQLite to a
    // shape we don't model — graphite declines rather than mis-render.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT * FROM (VALUES(1))",
        "SELECT * FROM (VALUES((SELECT 1),2),(3,4))",
        "SELECT * FROM (VALUES(1,2),(3,4)) t, (VALUES(5)) u",
        "SELECT * FROM (VALUES(1,2),(3,4)) t JOIN (VALUES(5,6)) u",
        "SELECT DISTINCT column1 FROM (VALUES(1),(2),(2)) ORDER BY column1",
        "SELECT * FROM (SELECT * FROM (VALUES(1,2),(3,4)))",
    ] {
        let sql = format!("EXPLAIN QUERY PLAN {q}");
        let got = run(g, &sql);
        assert!(
            got.contains("EXPLAIN QUERY PLAN for this query shape"),
            "{q} should decline as unsupported, got {got:?}"
        );
    }
}
