//! The expression walker that the prepare-time checks share (`window::visit`)
//! and the aggregate classifier/substitutor (`expr_contains_agg`,
//! `substitute_aggregates`) historically stopped at two sub-expression-bearing
//! `Expr` nodes — a row value `(a, b, …)` and `expr COLLATE name`. Anything
//! nested under those was invisible: a wrong-arity or unknown function went
//! unresolved, an aggregate/window misuse went unreported, and even a *valid*
//! aggregate wrapped in `COLLATE` (`sum(a) COLLATE binary`) was misclassified as
//! a scalar call and rejected as a misuse. Completing the descent closes all of
//! these against sqlite3 3.50.4.
//!
//! As elsewhere in the resolution suites, `lower`/`upper`/`substr` are avoided
//! because a locally ICU-enabled sqlite gives them an extra optional argument
//! (see the project's ICU note), which is not differentiable.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// First non-caret line of combined stdout/stderr, error-prefix stripped.
fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().next().unwrap_or("").trim_end().to_string();
    if !line.is_empty() {
        return line;
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
fn rowvalue_and_collate_resolution_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let e = "CREATE TABLE t(a,b);";
    let p = "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(3,4);";
    for sql in [
        // Wrong-arity / unknown function nested inside a row value — rejected at
        // prepare time even over an empty table.
        &format!("{e} SELECT a FROM t WHERE (abs(a,b),1)=(1,2)"),
        &format!("{e} SELECT a FROM t WHERE (nope(a),1)=(1,2)"),
        // … and nested under COLLATE.
        &format!("{e} SELECT abs(a,b) COLLATE nocase FROM t"),
        &format!("{e} SELECT nope(a) COLLATE nocase FROM t"),
        // Aggregate misuse nested in a row value / under COLLATE in WHERE.
        &format!("{e} SELECT a FROM t WHERE (count(*),1)=(1,1)"),
        &format!("{e} SELECT a FROM t WHERE count(*) COLLATE nocase"),
        // Window misuse nested in a row value in WHERE.
        &format!("{e} SELECT a FROM t WHERE (row_number() OVER (),1)=(1,1)"),
        // A *valid* aggregate wrapped in COLLATE is classified as aggregate and
        // computed (no spurious "misuse"): the whole-table and grouped forms.
        &format!("{p} SELECT sum(a) COLLATE binary FROM t"),
        &format!("{p} SELECT count(*) COLLATE nocase FROM t"),
        &format!("{p} SELECT max(a) COLLATE binary, min(b) FROM t"),
        &format!("{p} SELECT sum(a)+1 COLLATE binary FROM t"),
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(1,4); \
         SELECT a FROM t GROUP BY a HAVING sum(b) COLLATE binary > 3",
        // A valid row-value comparison still runs unchanged.
        &format!("{p} SELECT a FROM t WHERE (a,b)=(1,2)"),
        // A valid COLLATE on a plain column still runs unchanged.
        "CREATE TABLE t(a,b); INSERT INTO t VALUES('B','a'); \
         SELECT a COLLATE nocase FROM t ORDER BY 1",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
