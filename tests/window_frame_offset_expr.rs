//! A window frame offset (`<expr> PRECEDING` / `<expr> FOLLOWING`) may be any
//! constant expression — `(1+1)`, `2.0`, `CAST(2 AS INT)`, `2 COLLATE NOCASE`,
//! `1<<1` — not just an integer literal. SQLite evaluates the offset once at run
//! time, applies numeric affinity (so `'2'`/`'2.0'` work but `'2x'`, a blob, or
//! NULL fail), and requires a non-negative integer for `ROWS`/`GROUPS` or a
//! non-negative number for `RANGE`. A row-dependent offset (a column, a function
//! call, a subquery) is rejected with the same message, and the check is deferred
//! over an empty partition. graphite previously rejected every non-integer-literal
//! offset at parse time (`near "PRECEDING": syntax error`). Verified against the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

const SETUP: &str = "CREATE TABLE t(x); INSERT INTO t VALUES(1),(2),(3),(4),(5);";

/// First-column integers of a query, for pinning behavior without an oracle.
fn col0(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|row| match row[0] {
            Value::Integer(i) => i,
            ref v => panic!("expected int, got {v:?} from {sql}"),
        })
        .collect()
}

#[test]
fn expression_offsets_compute() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3),(4),(5)")
        .unwrap();
    // `ROWS (1+1) PRECEDING` is a 3-wide trailing window: sum of x over the row
    // and its two predecessors.
    assert_eq!(
        col0(
            &c,
            "SELECT sum(x) OVER (ORDER BY x ROWS (1+1) PRECEDING) FROM t ORDER BY x",
        ),
        vec![1, 3, 6, 9, 12],
    );
    // Equivalent ways of writing the offset 2.
    for off in [
        "2.0",
        "CAST(2 AS INT)",
        "2 COLLATE NOCASE",
        "-(-2)",
        "1<<1",
        "'2'",
        "'2.0'",
    ] {
        assert_eq!(
            col0(
                &c,
                &format!("SELECT sum(x) OVER (ORDER BY x ROWS {off} PRECEDING) FROM t ORDER BY x"),
            ),
            vec![1, 3, 6, 9, 12],
            "offset {off}",
        );
    }
}

#[test]
fn bad_offsets_are_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3),(4),(5)")
        .unwrap();
    // ROWS/GROUPS need a non-negative integer; a non-numeric text, a blob, a
    // fractional real, a negative, NULL, a row reference, a function, or a
    // subquery all fail.
    for off in [
        "'2x'",
        "2.5",
        "-1",
        "NULL",
        "x'32'",
        "x",
        "abs(2)",
        "(SELECT 2)",
    ] {
        let e = c
            .query(&format!(
                "SELECT sum(x) OVER (ORDER BY x ROWS {off} PRECEDING) FROM t"
            ))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("frame starting offset must be a non-negative integer"),
            "offset {off} gave {e:?}",
        );
    }
    // RANGE allows fractions but still rejects non-numbers, with "number" wording.
    let e = c
        .query("SELECT sum(x) OVER (ORDER BY x RANGE 'a' PRECEDING) FROM t")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("frame starting offset must be a non-negative number"),
        "{e:?}",
    );
}

#[test]
fn offset_validation_is_deferred_over_empty_input() {
    // No rows means no offset evaluation, so even a nonsense offset is accepted
    // and yields no rows — matching SQLite's run-time (stepping) validation.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    let r = c
        .query("SELECT sum(x) OVER (ORDER BY x ROWS 'bad' PRECEDING) FROM t")
        .unwrap();
    assert!(r.rows.is_empty());
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        // Normalize the CLI error decoration: graphite prints `error: <msg>`,
        // the sqlite3 shell prints `Error: stepping, <msg>` for run-time
        // failures. Compare the bare message.
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_start_matches("stepping, ")
            .trim_end()
            .to_string()
    };
    for sql in [
        // Constant-expression offsets that compute.
        "SELECT x, sum(x) OVER (ORDER BY x ROWS (1+1) PRECEDING) FROM t ORDER BY x",
        "SELECT x, sum(x) OVER (ORDER BY x ROWS 2.0 PRECEDING) FROM t ORDER BY x",
        "SELECT x, sum(x) OVER (ORDER BY x ROWS CAST(2 AS INT) PRECEDING) FROM t ORDER BY x",
        "SELECT x, sum(x) OVER (ORDER BY x ROWS 2 COLLATE NOCASE PRECEDING) FROM t ORDER BY x",
        "SELECT x, sum(x) OVER (ORDER BY x ROWS 1<<1 PRECEDING) FROM t ORDER BY x",
        "SELECT x, sum(x) OVER (ORDER BY x GROUPS (1*1) PRECEDING) FROM t ORDER BY x",
        "SELECT x, sum(x) OVER (ORDER BY x RANGE (2.5-0.5) PRECEDING) FROM t ORDER BY x",
        "SELECT x, sum(x) OVER (ORDER BY x RANGE 1.5 PRECEDING) FROM t ORDER BY x",
        "SELECT x, sum(x) OVER (ORDER BY x ROWS BETWEEN (1+0) PRECEDING AND (1+1) FOLLOWING) \
         FROM t ORDER BY x",
        // Numeric affinity on a text offset.
        "SELECT x, sum(x) OVER (ORDER BY x ROWS '2' PRECEDING) FROM t ORDER BY x",
        "SELECT x, sum(x) OVER (ORDER BY x ROWS '2.0' PRECEDING) FROM t ORDER BY x",
        // Run-time rejections (same message, modulo shell decoration).
        "SELECT sum(x) OVER (ORDER BY x ROWS '2x' PRECEDING) FROM t",
        "SELECT sum(x) OVER (ORDER BY x ROWS 2.5 PRECEDING) FROM t",
        "SELECT sum(x) OVER (ORDER BY x ROWS -1 PRECEDING) FROM t",
        "SELECT sum(x) OVER (ORDER BY x ROWS NULL PRECEDING) FROM t",
        "SELECT sum(x) OVER (ORDER BY x ROWS x PRECEDING) FROM t",
        "SELECT sum(x) OVER (ORDER BY x ROWS abs(2) PRECEDING) FROM t",
        "SELECT sum(x) OVER (ORDER BY x ROWS (SELECT 2) PRECEDING) FROM t",
        "SELECT sum(x) OVER (ORDER BY x RANGE 'a' PRECEDING) FROM t",
        "SELECT sum(x) OVER (ORDER BY x ROWS BETWEEN CURRENT ROW AND 'z' FOLLOWING) FROM t",
    ] {
        let full = format!("{SETUP} {sql}");
        assert_eq!(run("sqlite3", &full), run(g, &full), "for {sql}");
    }
}
