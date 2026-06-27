//! A window frame of type `RANGE` with a value offset bound (`<n> PRECEDING`
//! or `<n> FOLLOWING`, as opposed to `UNBOUNDED` or `CURRENT ROW`) compares the
//! `ORDER BY` value plus or minus the offset, so SQLite requires exactly one
//! `ORDER BY` expression in the window — zero or several both raise
//! `RANGE with offset PRECEDING/FOLLOWING requires one ORDER BY expression`.
//! graphite previously ignored the constraint and produced rows. `ROWS`/`GROUPS`
//! offsets are positional and carry no such requirement. Verified against the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn err(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

const MSG: &str = "RANGE with offset PRECEDING/FOLLOWING requires one ORDER BY expression";

#[test]
fn range_offset_without_one_order_by_is_rejected() {
    let c = Connection::open_memory().unwrap();
    // Zero ORDER BY expressions.
    for sql in [
        "SELECT sum(x) OVER (RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM (SELECT 1 x)",
        "SELECT sum(x) OVER (RANGE 1 PRECEDING) FROM (SELECT 1 x)",
        "SELECT sum(x) OVER (RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM (SELECT 1 x)",
        "SELECT sum(x) OVER (RANGE BETWEEN CURRENT ROW AND 1 FOLLOWING) FROM (SELECT 1 x)",
        // Two ORDER BY expressions is just as wrong as none.
        "SELECT sum(x) OVER (ORDER BY x, x RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
         FROM (SELECT 1 x)",
        // A named window carries the same rule.
        "SELECT sum(x) OVER w FROM (SELECT 1 x) WINDOW w AS (RANGE 1 PRECEDING)",
    ] {
        assert_eq!(err(&c, sql), MSG, "for {sql}");
    }
}

#[test]
fn well_formed_or_exempt_frames_are_accepted() {
    let c = Connection::open_memory().unwrap();
    for sql in [
        // Exactly one ORDER BY expression — the well-formed case.
        "SELECT sum(x) OVER (ORDER BY x RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
         FROM (SELECT 1 x)",
        // RANGE without an offset bound: no ORDER BY needed.
        "SELECT sum(x) OVER (RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
         FROM (SELECT 1 x)",
        // ROWS / GROUPS offsets are positional — exempt regardless of ORDER BY.
        "SELECT sum(x) OVER (ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM (SELECT 1 x)",
        "SELECT sum(x) OVER (GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM (SELECT 1 x)",
    ] {
        assert!(c.query(sql).is_ok(), "for {sql}");
    }
}

#[test]
fn well_formed_range_offset_frame_still_computes() {
    // The guard must not disturb a valid frame: sum over [v-1, v+1] by value.
    let c = Connection::open_memory().unwrap();
    let r = c
        .query(
            "SELECT x, sum(x) OVER (ORDER BY x RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
             FROM (SELECT 1 x UNION SELECT 2 UNION SELECT 5) ORDER BY x",
        )
        .unwrap();
    // x=1 -> {1,2}=3 ; x=2 -> {1,2}=3 ; x=5 -> {5}=5
    let got: Vec<(i64, i64)> = r
        .rows
        .into_iter()
        .map(|mut row| {
            let s = match row.remove(1) {
                graphitesql::Value::Integer(i) => i,
                other => panic!("{other:?}"),
            };
            let x = match row.remove(0) {
                graphitesql::Value::Integer(i) => i,
                other => panic!("{other:?}"),
            };
            (x, s)
        })
        .collect();
    assert_eq!(got, vec![(1, 3), (2, 3), (5, 5)]);
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
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "SELECT sum(x) OVER (RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM (SELECT 1 x)",
        "SELECT sum(x) OVER (RANGE 1 PRECEDING) FROM (SELECT 1 x)",
        "SELECT sum(x) OVER (RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM (SELECT 1 x)",
        "SELECT sum(x) OVER (ORDER BY x, x RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
         FROM (SELECT 1 x)",
        "SELECT sum(x) OVER (ORDER BY x RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
         FROM (SELECT 1 x)",
        "SELECT sum(x) OVER (RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM (SELECT 1 x)",
        "SELECT sum(x) OVER (ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM (SELECT 1 x)",
        "SELECT sum(x) OVER (GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM (SELECT 1 x)",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
