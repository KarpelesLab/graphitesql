//! A scalar function (or a multi-argument `min`/`max`, which is scalar) used
//! with an `OVER (…)` clause is **not** a window function — SQLite rejects it at
//! prepare time with `NAME() may not be used as a window function`. Only the
//! eleven built-in window functions (row_number, rank, dense_rank, percent_rank,
//! cume_dist, ntile, first_value, last_value, nth_value, lag, lead) and the true
//! aggregates (which double as window functions) are legal there.
//!
//! Two code paths must agree: a window query over a real base table runs through
//! the VDBE window path, while one whose source is a subquery (or that otherwise
//! falls back) runs through the tree-walker. Both are checked here.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;

/// Run `sql` on a connection that already has `t(a, b)`; return the trimmed
/// error message, or `Ok(())` if it succeeded.
fn run(sql: &str) -> Result<(), String> {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.query(sql).map(|_| ()).map_err(|e| {
        e.to_string()
            .trim_start_matches("error: ")
            .trim_start_matches("SQL error: ")
            .to_string()
    })
}

fn misuse(name: &str, sql: &str) {
    let want = format!("{name}() may not be used as a window function");
    assert_eq!(run(sql).unwrap_err(), want, "{sql:?}");
}

#[test]
fn scalar_function_over_a_real_table_is_rejected() {
    // VDBE window path (real base table).
    misuse("abs", "SELECT abs(a) OVER () FROM t");
    misuse("upper", "SELECT upper(a) OVER () FROM t");
    misuse("length", "SELECT length(a) OVER () FROM t");
    misuse("typeof", "SELECT typeof(a) OVER () FROM t");
    misuse("coalesce", "SELECT coalesce(a, b) OVER () FROM t");
}

#[test]
fn multi_arg_min_max_over_a_real_table_is_rejected() {
    // `min`/`max` are aggregates with one argument but scalar with two, so the
    // two-argument forms are not window functions either.
    misuse("max", "SELECT max(a, b) OVER () FROM t");
    misuse("min", "SELECT min(a, b) OVER () FROM t");
}

#[test]
fn scalar_function_over_a_subquery_source_is_rejected() {
    // Tree-walker path (subquery source, no real base table).
    let c = Connection::open_memory().unwrap();
    let err = |sql: &str| {
        c.query(sql)
            .map(|_| ())
            .map_err(|e| {
                e.to_string()
                    .trim_start_matches("error: ")
                    .trim_start_matches("SQL error: ")
                    .to_string()
            })
            .unwrap_err()
    };
    assert_eq!(
        err("SELECT abs(a) OVER () FROM (SELECT 1 a)"),
        "abs() may not be used as a window function"
    );
    assert_eq!(
        err("SELECT max(a, b) OVER () FROM (SELECT 1 a, 2 b)"),
        "max() may not be used as a window function"
    );
}

#[test]
fn scalar_window_misuse_in_order_by_is_rejected() {
    misuse("abs", "SELECT a FROM t ORDER BY abs(a) OVER ()");
}

#[test]
fn genuine_window_functions_still_work() {
    let ok = |sql: &str| run(sql).unwrap_or_else(|e| panic!("expected {sql:?} to run, got: {e}"));
    // The eleven built-ins.
    ok("SELECT row_number() OVER (ORDER BY a) FROM t");
    ok("SELECT rank() OVER (ORDER BY a) FROM t");
    ok("SELECT first_value(a) OVER (ORDER BY a) FROM t");
    ok("SELECT lag(a) OVER (ORDER BY a) FROM t");
    // Aggregates double as window functions, including single-arg min/max.
    ok("SELECT sum(a) OVER () FROM t");
    ok("SELECT max(a) OVER () FROM t");
    ok("SELECT count(*) OVER () FROM t");
}
