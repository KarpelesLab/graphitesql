//! Statement-validation parity with SQLite: inputs the `sqlite3` CLI rejects at
//! prepare time, which graphite used to accept silently. We match SQLite on
//! *errors-vs-succeeds* (the CLI's message wording differs and is normalized
//! away), so each rejection case is cross-checked against the CLI when present.
//!
//!   * `VALUES (…),(…)` rows with differing arity (standalone, as a compound
//!     operand, and as `INSERT … VALUES`).
//!   * `GROUP BY` / `ORDER BY` positional terms outside `1..=ncols`.
//!   * `UNION`/`INTERSECT`/`EXCEPT` operands with mismatched column counts.
//!
//! Valid queries (including legitimate positional `GROUP BY` / `ORDER BY` and
//! `VALUES` of uniform arity) must keep working.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Does the `sqlite3` CLI reject `sql`? (Returns the boolean only when the CLI is
/// present; callers gate on [`sqlite3_available`].)
fn sqlite_rejects(setup: &[&str], sql: &str) -> bool {
    let mut script = String::new();
    for s in setup {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(sql);
    script.push(';');
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(&script)
        .output()
        .expect("run sqlite3");
    !out.status.success() || !String::from_utf8_lossy(&out.stderr).is_empty()
}

fn setup(c: &mut Connection) {
    c.execute("CREATE TABLE t(a, b, c)").unwrap();
    c.execute("INSERT INTO t VALUES(1, 2, 3), (4, 5, 6)")
        .unwrap();
}

/// Assert graphite rejects `sql`, and (when available) so does the CLI.
fn both_reject(c: &Connection, setup_sql: &[&str], sql: &str) {
    assert!(c.query(sql).is_err(), "graphite should reject: {sql}");
    if sqlite3_available() {
        assert!(
            sqlite_rejects(setup_sql, sql),
            "sqlite should reject: {sql}"
        );
    }
}

#[test]
fn values_rows_must_have_equal_arity() {
    let c = Connection::open_memory().unwrap();
    // Standalone VALUES.
    both_reject(&c, &[], "VALUES(1, 2), (3)");
    both_reject(&c, &[], "VALUES(1), (2, 3)");
    both_reject(&c, &[], "VALUES(1, 2, 3), (4, 5), (6, 7, 8)");
    // As a compound operand.
    both_reject(&c, &[], "SELECT 1, 2 UNION VALUES(1, 2), (3)");
    both_reject(&c, &[], "SELECT 1, 2 UNION ALL VALUES(7), (8, 9)");
}

#[test]
fn uniform_values_still_work() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        c.query("VALUES(1, 2), (3, 4)").unwrap().rows.len(),
        2,
        "uniform VALUES must still run"
    );
    assert_eq!(
        c.query("SELECT 1, 2 UNION VALUES(3, 4), (1, 2)")
            .unwrap()
            .rows
            .len(),
        2
    );
    assert_eq!(
        c.query("SELECT sum(column1) FROM (VALUES (1), (2), (3))")
            .unwrap()
            .rows[0][0],
        graphitesql::Value::Integer(6)
    );
}

#[test]
fn insert_values_arity_mismatch_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE x(a, b)").unwrap();
    assert!(
        c.execute("INSERT INTO x VALUES(1, 2), (3)").is_err(),
        "INSERT with a short VALUES row must error"
    );
    if sqlite3_available() {
        assert!(sqlite_rejects(
            &["CREATE TABLE x(a, b)"],
            "INSERT INTO x VALUES(1, 2), (3)"
        ));
    }
    // A uniform multi-row INSERT still works.
    c.execute("INSERT INTO x VALUES(1, 2), (3, 4)").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM x").unwrap().rows[0][0],
        graphitesql::Value::Integer(2)
    );
}

#[test]
fn positional_group_by_out_of_range_rejected() {
    let mut c = Connection::open_memory().unwrap();
    setup(&mut c);
    both_reject(&c, &[], "SELECT 1 GROUP BY 2");
    both_reject(&c, &[], "SELECT 1 GROUP BY 0");
    both_reject(&c, &[], "SELECT a, b FROM t GROUP BY 3");
    both_reject(&c, &[], "SELECT a FROM t GROUP BY -1");
}

#[test]
fn positional_order_by_out_of_range_rejected() {
    let mut c = Connection::open_memory().unwrap();
    setup(&mut c);
    both_reject(&c, &[], "SELECT 1 ORDER BY 2");
    both_reject(&c, &[], "SELECT 1 ORDER BY 0");
    both_reject(&c, &[], "SELECT 1 ORDER BY -1");
    both_reject(&c, &[], "SELECT 1 ORDER BY (2)");
    both_reject(&c, &[], "SELECT a, b FROM t ORDER BY 3");
    // In a compound query the ORDER BY refers to the output columns.
    both_reject(&c, &[], "SELECT a FROM t UNION SELECT b FROM t ORDER BY 2");
}

#[test]
fn positional_in_range_still_works() {
    let mut c = Connection::open_memory().unwrap();
    setup(&mut c);
    // ORDER BY positions resolve to output columns.
    assert_eq!(
        c.query("SELECT a, b, c FROM t ORDER BY 3 DESC")
            .unwrap()
            .rows[0][0],
        graphitesql::Value::Integer(4)
    );
    // `*` expands so the count reflects the real width.
    assert_eq!(c.query("SELECT * FROM t ORDER BY 3").unwrap().rows.len(), 2);
    // An in-range positional GROUP BY is accepted (not rejected as out of range).
    assert!(c
        .query("SELECT a, count(*) FROM t GROUP BY 1 ORDER BY 1")
        .is_ok());
    // An ORDER BY *expression* (not a bare integer) is never positional.
    assert_eq!(
        c.query("SELECT 5 ORDER BY 1 + 1").unwrap().rows[0][0],
        graphitesql::Value::Integer(5)
    );
}

#[test]
fn compound_column_count_mismatch_rejected() {
    let c = Connection::open_memory().unwrap();
    both_reject(&c, &[], "SELECT 1 UNION SELECT 1, 2");
    both_reject(&c, &[], "SELECT 1, 2 INTERSECT SELECT 1");
    both_reject(&c, &[], "SELECT 1 EXCEPT SELECT 1, 2, 3");
    both_reject(&c, &[], "SELECT 1 UNION ALL SELECT 1, 2");
}

#[test]
fn matching_compound_widths_still_work() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        c.query("SELECT 1, 2 UNION SELECT 3, 4").unwrap().rows.len(),
        2
    );
    assert_eq!(
        c.query("SELECT 1 UNION SELECT 1 UNION SELECT 2")
            .unwrap()
            .rows
            .len(),
        2
    );
}

#[test]
fn aggregate_misuse_still_rejected() {
    // These already errored in graphite; confirm they stay rejected and that
    // sqlite agrees.
    let mut c = Connection::open_memory().unwrap();
    setup(&mut c);
    both_reject(
        &c,
        &["CREATE TABLE t(a, b, c)"],
        "SELECT a FROM t WHERE count(*) > 0",
    );
    both_reject(
        &c,
        &["CREATE TABLE t(a, b, c)"],
        "SELECT count(count(*)) FROM t",
    );
    // HAVING on a non-aggregate query.
    both_reject(
        &c,
        &["CREATE TABLE t(a, b, c)"],
        "SELECT a FROM t HAVING a > 1",
    );
}
