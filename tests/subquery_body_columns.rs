//! A column reference inside an expression-position subquery body is resolved at
//! prepare time by SQLite: one that binds to neither the subquery's own `FROM`
//! nor any enclosing (correlation) scope is `no such column: <as-written>`,
//! reported even when the outer table is empty or every row is filtered out.
//! graphite resolves lazily, per row, so over such a result the subquery is
//! never reached and the error was silently missed (A-prepare-correlated). It
//! now matches: a missing two- or three-part reference errors at prepare, a
//! schema-qualified reference must also name the column's database of origin, and
//! every valid correlated reference still resolves.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The `no such column: …` tail of the error, however the CLI wraps it (the
/// library message is byte-identical; only graphite's `Error: error:` vs
/// sqlite's `Error: in prepare,` prefix and sqlite's caret diagram differ). For
/// a query that does not error, returns its first non-empty output line so the
/// valid cases compare equal too.
fn err_tail(bin: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for line in text.lines() {
        if let Some(pos) = line.find("no such column:") {
            return line[pos..].trim_end().to_string();
        }
    }
    text.lines()
        .map(str::trim_end)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

#[test]
fn subquery_body_column_resolution_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        // --- Errors: a body reference that binds to nothing in scope. ---
        // Wrong schema on an otherwise-valid column.
        "CREATE TABLE t(a); SELECT (SELECT bad.t.a) FROM t",
        // Table not in any scope (schema-qualified and bare).
        "CREATE TABLE t(a); SELECT (SELECT main.u.a) FROM t",
        "CREATE TABLE t(a); SELECT (SELECT u.a) FROM t",
        // Column the named table does not have.
        "CREATE TABLE t(a); SELECT (SELECT main.t.nope) FROM t",
        // In a WHERE-position scalar subquery, an IN subquery, and an EXISTS.
        "CREATE TABLE t(a); SELECT a FROM t WHERE (SELECT bad.t.a)=1",
        "CREATE TABLE t(a); SELECT a FROM t WHERE a IN (SELECT bad.t.a)",
        "CREATE TABLE t(a); SELECT a FROM t WHERE EXISTS(SELECT bad.t.a)",
        // A CTE column-list names its columns; a missing one errors.
        "WITH c(x) AS (SELECT 1) SELECT (SELECT c.y) FROM c",
        // A derived table in the outer FROM: its missing column errors.
        "CREATE TABLE t(a); SELECT (SELECT d.nope) FROM (SELECT a FROM t) d",
        // A column that exists only in a table NOT in scope.
        "CREATE TABLE t(a); CREATE TABLE u(b); SELECT (SELECT b) FROM t",
        // An aliased outer table hides its original name.
        "CREATE TABLE t(a); SELECT (SELECT t.a) FROM t z",
        // Outer fault wins over a body fault.
        "CREATE TABLE t(a); SELECT nope, (SELECT bad.t.a) FROM t",
        // A body missing column wins over an arity mismatch.
        "CREATE TABLE t(a,b); SELECT (SELECT bad.t.a, b FROM t) FROM t",
        // --- Valid correlated references: must NOT error. ---
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); SELECT (SELECT t.a) FROM t",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); SELECT (SELECT a) FROM t",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); SELECT (SELECT main.t.a) FROM t",
        // Genuine correlation: the subquery has its own FROM and reads the outer.
        "CREATE TABLE t(a); CREATE TABLE u(a); INSERT INTO t VALUES(1); \
         INSERT INTO u VALUES(9); SELECT (SELECT u.a FROM u WHERE u.a>t.a) FROM t",
        // Two-level nesting reaching the outermost table.
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT (SELECT (SELECT t.a)) FROM t",
        // Output-alias exemption (a bare ORDER BY reference to an alias).
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT (SELECT a AS z ORDER BY z) FROM t",
        // Attached-database correlation resolves with its real origin.
        "ATTACH ':memory:' AS aux; CREATE TABLE aux.u(a); INSERT INTO aux.u VALUES(5); \
         CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT (SELECT aux.u.a FROM aux.u) FROM t",
        // A two-table outer scope correlates both names.
        "CREATE TABLE t(a); CREATE TABLE u(b); INSERT INTO t VALUES(1); \
         INSERT INTO u VALUES(2); SELECT (SELECT t.a+u.b) FROM t,u",
        // A USING-coalesced column is in scope.
        "CREATE TABLE t(a,x); CREATE TABLE u(a,y); INSERT INTO t VALUES(1,10); \
         INSERT INTO u VALUES(1,20); SELECT (SELECT a) FROM t JOIN u USING(a)",
        // An aliased outer table is reachable by its alias, with its db.
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT (SELECT z.a) FROM t z",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT (SELECT main.z.a) FROM t z",
        // A valid scalar/aggregate correlated body still runs.
        "CREATE TABLE t(a); CREATE TABLE u(b); INSERT INTO t VALUES(1); \
         INSERT INTO u VALUES(2); SELECT (SELECT sum(b) FROM u WHERE b>t.a) FROM t",
    ] {
        assert_eq!(err_tail("sqlite3", sql), err_tail(g, sql), "for {sql}");
    }
}
