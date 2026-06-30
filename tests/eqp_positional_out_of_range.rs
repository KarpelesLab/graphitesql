//! `EXPLAIN QUERY PLAN` must reject an out-of-range positional `GROUP BY` / `ORDER BY`
//! ordinal at prepare time, exactly as the executed statement does and exactly as
//! SQLite does — `Nth <clause> term out of range - should be between 1 and M`. The
//! executed path runs this check in `run_core`; the plan path previously skipped it and
//! built a (meaningless) tree for the invalid query. `eqp_select` now runs the same
//! `check_positional_terms` for a wildcard-free projection (where the output-column
//! count is exactly the projected-column count). Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

const SETUP: &str = "CREATE TABLE t(a, b, c); INSERT INTO t VALUES(1,2,3);";

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

fn err(c: &Connection, sql: &str) -> String {
    let e = c.query(sql).unwrap_err().to_string();
    e.strip_prefix("error: ").unwrap_or(&e).to_string()
}

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// sqlite errors for the EXPLAIN'd query too — confirm the oracle agrees that the plan
/// path is a prepare-time error (a stdout with no "Error:" would mean it built a plan).
fn sqlite_errors(sql: &str) -> bool {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{SETUP} EXPLAIN QUERY PLAN {sql};"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stderr).contains("out of range")
        || String::from_utf8_lossy(&o.stdout).contains("out of range")
}

#[test]
fn eqp_rejects_out_of_range_positional() {
    let c = conn();
    for (sql, msg) in [
        (
            "SELECT a FROM t ORDER BY 2",
            "1st ORDER BY term out of range - should be between 1 and 1",
        ),
        (
            "SELECT a, b FROM t ORDER BY 3",
            "1st ORDER BY term out of range - should be between 1 and 2",
        ),
        (
            "SELECT a FROM t ORDER BY -1",
            "1st ORDER BY term out of range - should be between 1 and 1",
        ),
        (
            "SELECT a FROM t GROUP BY 2",
            "1st GROUP BY term out of range - should be between 1 and 1",
        ),
        // ORDER BY is resolved before GROUP BY, so its out-of-range term is reported.
        (
            "SELECT a, b FROM t GROUP BY 1 ORDER BY 5",
            "1st ORDER BY term out of range - should be between 1 and 2",
        ),
    ] {
        assert_eq!(
            err(&c, &format!("EXPLAIN QUERY PLAN {sql}")),
            msg,
            "for {sql}"
        );
        // The same error fires for the executed statement.
        assert_eq!(err(&c, sql), msg, "executed {sql}");
        if have_sqlite() {
            assert!(sqlite_errors(sql), "sqlite should also reject {sql}");
        }
    }
}

#[test]
fn eqp_allows_in_range_positional() {
    let c = conn();
    // An in-range ordinal builds a plan, no error — both the plan and executed paths.
    for sql in [
        "SELECT a FROM t ORDER BY 1",
        "SELECT a, b FROM t ORDER BY 2",
        "SELECT a, b, c FROM t GROUP BY 1, 2, 3",
        "SELECT a, b FROM t GROUP BY 1 ORDER BY 2",
    ] {
        let plan = c
            .query(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap_or_else(|e| panic!("{sql} should plan, got {e}"));
        assert!(!plan.rows.is_empty(), "{sql} produced no plan rows");
        c.query(sql)
            .unwrap_or_else(|e| panic!("{sql} should run, got {e}"));
    }
}
