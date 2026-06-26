//! SQLite resolves every column reference at prepare time, so a reference to a
//! non-existent column errors with `no such column: …` *regardless of the data*
//! — even when the table is empty, or when a `WHERE` clause filters out every
//! row, so projection/`WHERE` evaluation never actually touches the bad name.
//!
//! graphite's tree-walker resolves columns lazily, during per-row evaluation, so
//! a result that reaches no row used to silently swallow the error and return an
//! empty result set. `Executor::validate_columns_exist` closes that gap for the
//! cases it can settle without any chance of a false positive: a bare/qualified
//! reference in the projection or `WHERE` of a top-level, window-free block whose
//! every `FROM` source is a plain (non-virtual, non-subquery, non-TVF) base
//! table/view joined only by `ON`/cross joins (no `NATURAL`/`USING`).
//!
//! Each case is checked against the real sqlite3 CLI: graphite must error exactly
//! when sqlite does, and a *valid* query over the same empty/filtered data must
//! still succeed (the check must never reject a real column).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE e(a INT, b TEXT);\n\
    CREATE TABLE t(a INT, b TEXT);\n\
    INSERT INTO t VALUES (1,'x'),(2,'y');\n\
    CREATE TABLE u(c INT, d TEXT);\n\
    INSERT INTO u VALUES (1,'p');\n\
    CREATE VIEW v AS SELECT a AS va, b AS vb FROM t;\n";

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    c
}

/// `true` if the real sqlite3 CLI reports an error for `query` over `SETUP`.
fn sqlite_errors(query: &str) -> bool {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg("-ascii")
        .arg(format!("{SETUP}{query};"))
        .output()
        .unwrap();
    // sqlite3 prints the error to stderr and exits non-zero.
    !out.status.success() || !out.stderr.is_empty()
}

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn missing_column_errors_match_sqlite() {
    let queries = &[
        // Empty table: projection references a column that does not exist.
        "SELECT nope FROM e",
        // Empty table: qualified missing column.
        "SELECT e.nope FROM e",
        // Empty table: missing column only in WHERE.
        "SELECT a FROM e WHERE nope = 1",
        // Non-empty table but WHERE filters out every row before projection.
        "SELECT nope FROM t WHERE 0",
        "SELECT a, nope FROM t WHERE a > 100",
        // Missing column inside a function argument.
        "SELECT count(nope) FROM e",
        "SELECT upper(nope) FROM t WHERE 0",
        // Missing column inside CASE / arithmetic / IN list.
        "SELECT CASE WHEN nope THEN 1 END FROM t WHERE 0",
        "SELECT a + nope FROM t WHERE 0",
        "SELECT a FROM t WHERE a IN (nope, 1)",
        // Qualified by a real table but the column is wrong.
        "SELECT t.nope FROM t",
        // Two-table ON join: a missing column in the projection.
        "SELECT t.a, nope FROM t JOIN u ON t.a = u.c WHERE 0",
        // Wrong qualifier on an otherwise-real column name.
        "SELECT u.a FROM t JOIN u ON t.a = u.c",
        // Missing column referencing a view's (renamed) output.
        "SELECT b FROM v",
    ];
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    for q in queries {
        let graphite_err = c.query(q).is_err();
        let sqlite_err = sqlite_errors(q);
        assert!(
            sqlite_err,
            "test bug: expected sqlite3 to reject `{q}` but it did not"
        );
        assert!(
            graphite_err,
            "graphite accepted `{q}` but sqlite3 rejects it (missing-column not caught eagerly)"
        );
    }
}

#[test]
fn valid_queries_over_empty_or_filtered_data_still_succeed() {
    // The eager check must never reject a *real* column, including over an empty
    // table, a fully-filtered result, a view's renamed outputs, rowid aliases,
    // and date/time keyword pseudo-columns.
    let queries = &[
        "SELECT a FROM e",
        "SELECT a, b FROM e",
        "SELECT e.a, e.b FROM e",
        "SELECT a FROM t WHERE 0",
        "SELECT a, b FROM t WHERE a > 100",
        "SELECT count(a) FROM e",
        "SELECT rowid, a FROM e",
        "SELECT _rowid_ FROM t WHERE 0",
        "SELECT va, vb FROM v",
        "SELECT v.va FROM v WHERE 0",
        "SELECT t.a, u.d FROM t JOIN u ON t.a = u.c WHERE 0",
        "SELECT CURRENT_DATE FROM e",
    ];
    let c = conn();
    for q in queries {
        assert!(
            c.query(q).is_ok(),
            "graphite wrongly rejected the valid query `{q}`"
        );
    }
}

#[test]
fn subquery_and_natural_join_paths_are_left_to_lazy_resolution() {
    // These shapes are deliberately *outside* the eager check's conservative
    // scope (correlated outer refs, NATURAL/USING coalescing, derived tables).
    // They must still execute correctly — the guards must not make the check fire
    // and wrongly reject a legitimate reference.
    let c = conn();
    let ok = &[
        // USING coalesces `a`; both sides legitimately expose it.
        "SELECT a FROM t JOIN t AS t2 USING (a) WHERE 0",
        // Derived table: the inner alias is the only valid scope for `x`.
        "SELECT x FROM (SELECT a AS x FROM t) WHERE 0",
        // Correlated subquery referencing the outer table by qualifier.
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.c = t.a) AND 0",
    ];
    for q in ok {
        assert!(
            c.query(q).is_ok(),
            "graphite wrongly rejected the valid query `{q}`"
        );
    }
}
