//! A `FILTER (WHERE …)` clause is only meaningful on an *aggregate* window
//! function. Attaching it to a ranking/value window function (`rank`,
//! `row_number`, `first_value`, `lag`, …) is rejected by SQLite at prepare time
//! with `FILTER clause may only be used with aggregate window functions`.
//! graphite previously ignored the FILTER and returned the unfiltered result.
//! `FILTER` on an aggregate window function (`sum`/`count`/… `OVER (…)`) stays
//! valid. Verified against the sqlite3 3.50.4 CLI.

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

const DDL: &str = "CREATE TABLE t(id INT, g TEXT, v INT); \
     INSERT INTO t VALUES(1,'a',10),(2,'a',20),(3,'a',30),\
     (4,'b',30),(5,'b',5),(6,'b',7),(7,'b',15);";

#[test]
fn filter_on_window_function_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for tail in [
        // Non-aggregate window functions with FILTER: all rejected.
        "SELECT id, rank() FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        "SELECT id, dense_rank() FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        "SELECT id, row_number() FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        "SELECT id, percent_rank() FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        "SELECT id, cume_dist() FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        "SELECT id, ntile(2) FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        "SELECT id, first_value(v) FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        "SELECT id, last_value(v) FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        "SELECT id, nth_value(v,2) FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        "SELECT id, lag(v) FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        "SELECT id, lead(v) FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        // Aggregate window functions with FILTER stay valid.
        "SELECT id, sum(v) FILTER (WHERE v>5) OVER (ORDER BY id) FROM t;",
        "SELECT id, count(*) FILTER (WHERE v>5) OVER (PARTITION BY g) FROM t;",
        "SELECT id, avg(v) FILTER (WHERE v>5) OVER (PARTITION BY g) FROM t;",
    ] {
        let sql = format!("{DDL} {tail}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {tail}");
    }
}
