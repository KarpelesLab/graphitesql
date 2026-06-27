//! An aggregate inside a window function's `OVER` spec (`PARTITION BY` /
//! `ORDER BY`) — e.g. `row_number() OVER (ORDER BY sum(a))` — makes the
//! enclosing query a single aggregate group that is computed *before* the
//! window runs, so the window then sees one row. SQLite routes such a query
//! through its aggregate machinery even without a GROUP BY or a plain result
//! aggregate. graphite used to treat the over spec as non-aggregate, so it ran
//! the plain-window path over the raw rows and produced one result row per
//! input row instead of a single grouped row.
//!
//! Crucially, an over-spec aggregate does NOT make the query an aggregate one
//! for HAVING-validity: `... OVER (ORDER BY sum(a)) ... HAVING sum(a)>0` still
//! raises `HAVING clause on a non-aggregate query` unless a *real* (non-windowed)
//! aggregate appears in the result columns. Verified vs sqlite3 3.50.4.
#![cfg(feature = "std")]

use std::process::Command;

fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(sql)
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if !line.is_empty() {
            return line.to_string();
        }
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr.lines() {
        if line.starts_with('^') {
            continue;
        }
        let s = line
            .strip_prefix("Error: in prepare, ")
            .or_else(|| line.strip_prefix("Error: stepping, "))
            .or_else(|| line.strip_prefix("Error: SQL error: "))
            .or_else(|| line.strip_prefix("Error: "))
            .unwrap_or(line);
        let s = s.strip_prefix("error: ").unwrap_or(s);
        let s = s.rsplit_once(" (").map_or(s, |(head, tail)| {
            if tail
                .trim_end_matches(')')
                .chars()
                .all(|c| c.is_ascii_digit())
            {
                head
            } else {
                s
            }
        });
        return s.to_string();
    }
    String::new()
}

fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn same(sql: &str) {
    let g = run(env!("CARGO_BIN_EXE_graphitesql"), sql);
    let s = run("sqlite3", sql);
    assert_eq!(g, s, "mismatch for SQL: {sql}");
}

const SETUP: &str = "CREATE TABLE t(a,g); INSERT INTO t VALUES(1,10),(2,10),(3,20);";

#[test]
fn over_spec_aggregate_collapses_to_single_group() {
    if !sqlite3_available() {
        return;
    }
    // An aggregate in ORDER BY / PARTITION BY of the over spec → one grouped row.
    same(&format!(
        "{SETUP} SELECT row_number() OVER (ORDER BY sum(a)) FROM t;"
    ));
    same(&format!(
        "{SETUP} SELECT row_number() OVER (PARTITION BY sum(a)) FROM t;"
    ));
    same(&format!(
        "{SETUP} SELECT row_number() OVER (PARTITION BY count(*) ORDER BY sum(a)) FROM t;"
    ));
    same(&format!(
        "{SETUP} SELECT rank() OVER (ORDER BY avg(a), max(g)) FROM t;"
    ));
    // A bare column alongside the window still collapses to the single group.
    same(&format!(
        "{SETUP} SELECT a, row_number() OVER (ORDER BY sum(a)) FROM t;"
    ));
    // The over-spec aggregate nested inside a larger result expression.
    same(&format!(
        "{SETUP} SELECT 1 + row_number() OVER (ORDER BY sum(a)) FROM t;"
    ));
    same(&format!(
        "{SETUP} SELECT CASE WHEN 1 THEN row_number() OVER (ORDER BY sum(a)) END FROM t;"
    ));
    // Empty table → still one group.
    same("CREATE TABLE t(a,g); SELECT row_number() OVER (ORDER BY sum(a)) FROM t;");
}

#[test]
fn over_spec_aggregate_composes_with_group_by_and_postprocess() {
    if !sqlite3_available() {
        return;
    }
    // GROUP BY still partitions; the window runs per group.
    same(&format!(
        "{SETUP} SELECT row_number() OVER (ORDER BY sum(a)) FROM t GROUP BY g;"
    ));
    // DISTINCT / ORDER BY / LIMIT post-processing over the single grouped row.
    same(&format!(
        "{SETUP} SELECT DISTINCT row_number() OVER (ORDER BY sum(a)) FROM t;"
    ));
    same(&format!(
        "{SETUP} SELECT row_number() OVER (ORDER BY sum(a)) AS r FROM t ORDER BY r LIMIT 1;"
    ));
}

#[test]
fn over_spec_aggregate_does_not_make_having_valid() {
    if !sqlite3_available() {
        return;
    }
    // An over-spec aggregate is NOT a result aggregate: HAVING without a real
    // result aggregate is rejected, exactly as SQLite does (even HAVING 1 / 0).
    same(&format!(
        "{SETUP} SELECT row_number() OVER (ORDER BY sum(a)) FROM t HAVING sum(a)>0;"
    ));
    same(&format!(
        "{SETUP} SELECT row_number() OVER (ORDER BY sum(a)) FROM t HAVING 1;"
    ));
    same(&format!(
        "{SETUP} SELECT row_number() OVER (ORDER BY sum(a)) FROM t HAVING 0;"
    ));
    same(&format!("{SETUP} SELECT count(*) OVER () FROM t HAVING 1;"));
    // A real (non-windowed) result aggregate DOES make HAVING valid.
    same(&format!(
        "{SETUP} SELECT sum(a), row_number() OVER (ORDER BY count(*)) FROM t HAVING sum(a)>0;"
    ));
}
