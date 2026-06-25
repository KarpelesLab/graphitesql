//! Differential value-semantics regression suite.
//!
//! A curated batch of edge cases — numeric formatting & overflow, type affinity
//! and `CAST`, string functions, date/time, comparison / NULL / `IN` semantics,
//! JSON, and window functions — each run through both graphitesql and the
//! `sqlite3` CLI and asserted byte-equal in SQLite's default list mode. These
//! all pass today; the suite exists to *keep* them passing (the "probe the
//! corpus blind spots" strategy from ROADMAP §6, frozen into a regression gate).
//!
//! Deliberately excluded: introspection whose result set is build-specific in
//! the pinned `alt1` oracle (`PRAGMA function_list` / `collation_list` enumerate
//! that build's loaded extensions), and error *wording* (the engine messages
//! match, but the `sqlite3` CLI wraps them as `Runtime error near line N: … (19)`
//! — a CLI artifact, not an engine value). See ROADMAP §6.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Render one graphitesql result the way `sqlite3` prints default list mode:
/// cells joined by `|`, rows by `\n`, NULL as empty, blobs as lowercase hex,
/// reals via the canonical `%.15g` formatting.
fn render(result: &graphitesql::QueryResult) -> String {
    let mut lines = Vec::new();
    for row in &result.rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| match v {
                Value::Null => String::new(),
                Value::Integer(i) => i.to_string(),
                Value::Text(s) => s.clone(),
                Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
            })
            .collect();
        lines.push(cells.join("|"));
    }
    lines.join("\n")
}

fn graphite(sql: &str) -> String {
    let c = Connection::open_memory().unwrap();
    render(&c.query(sql).unwrap())
}

fn sqlite(sql: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

/// Assert every query in `cases` renders identically in both engines.
fn assert_match(cases: &[&str]) {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    for &sql in cases {
        assert_eq!(graphite(sql), sqlite(sql), "diverged on: {sql}");
    }
}

#[test]
fn numeric_formatting_and_overflow() {
    assert_match(&[
        "SELECT printf('%.3f', 2.0/3)",
        "SELECT printf('%+d %x %o', 5, 255, 8)",
        "SELECT printf('%5.2f|%-5d|', 3.14159, 7)",
        "SELECT printf('%e', 12345.678)",
        "SELECT printf('%g', 0.0001)",
        "SELECT printf('%g', 100000.0)",
        "SELECT printf('%c', 65)",
        "SELECT 9223372036854775807 + 1",
        "SELECT -9223372036854775808 / -1",
        "SELECT 1.0/0.0, -1.0/0.0, 0.0/0.0",
        "SELECT cast(1e19 as integer)",
        "SELECT cast(9.9e18 as integer)",
        "SELECT round(2.5), round(3.5), round(-2.5)",
        "SELECT round(1.2345, 2), round(1.005, 2)",
        "SELECT 5%0, 5.5%2, 0%5",
    ]);
}

#[test]
fn affinity_cast_and_comparison() {
    assert_match(&[
        "SELECT '10'+5, '10abc'+5, 'abc'+5",
        "SELECT '0x10'+0, '  12  '+0, '1e3'+0",
        "SELECT 1='1', 1=1.0, '1'='1.0', 1.0='1'",
        "SELECT cast('  12  ' as integer), cast('12.9' as integer), cast('1e3' as integer)",
        "SELECT typeof(1+1), typeof(1.0+1), typeof('1'+1), typeof(1/1), typeof(7/2)",
        "SELECT 'a' < 'B', 'a' < 'b', 'Z' < 'a'",
        "SELECT 2>'10', 2>10, '2'>'10'",
        "SELECT x'41' = 'A', x'41' < x'42'",
        "SELECT max(1,'a',2.5,NULL), min('b',1,NULL)",
        "SELECT NULL IS NULL, NULL IS NOT NULL, NULL=NULL, NULL<>NULL",
        "SELECT 1 IN (1,2,NULL), 3 IN (1,2,NULL), NULL IN (1,2)",
        "SELECT coalesce(NULL,NULL,3), ifnull(NULL,'x'), nullif(1,1), nullif(1,2)",
    ]);
}

#[test]
fn string_functions() {
    assert_match(&[
        "SELECT substr('hello', -3, 2)",
        "SELECT substr('hello', 0)",
        "SELECT replace('aaa', '', 'b')",
        "SELECT instr('hello', '')",
        "SELECT quote(x'00ff'), quote('a''b'), quote(3.14), quote(null)",
        "SELECT char(0x1F600)",
        "SELECT unicode('\u{1F600}'), length('\u{1F600}'), length(cast('\u{1F600}' as blob))",
        "SELECT hex(zeroblob(3)), typeof(zeroblob(0))",
        "SELECT ltrim('xxabc','x'), rtrim('abcyy','y')",
        "SELECT upper('caf\u{e9}'), lower('CAF\u{c9}')",
    ]);
}

#[test]
fn date_and_time() {
    assert_match(&[
        "SELECT date('2020-02-29','+1 year')",
        "SELECT date('2020-01-31','+1 month')",
        "SELECT strftime('%Y-%W-%w','2024-01-01')",
        "SELECT julianday('2000-01-01')",
        "SELECT strftime('%s','1970-01-01 00:00:01')",
        "SELECT datetime(0,'unixepoch')",
        "SELECT strftime('%j','2024-03-01')",
    ]);
}

#[test]
fn aggregates_and_null() {
    assert_match(&[
        "SELECT sum(x) FROM (SELECT 1 x WHERE 0)",
        "SELECT total(x) FROM (SELECT 1 x WHERE 0)",
        "SELECT avg(x) FROM (SELECT 1 x UNION SELECT NULL)",
        "SELECT group_concat(x,'') FROM (SELECT 1 x UNION SELECT 2)",
        "SELECT count(DISTINCT x) FROM (SELECT 1 x UNION SELECT 1 UNION SELECT NULL)",
    ]);
}

#[test]
fn json_functions() {
    assert_match(&[
        "SELECT json_extract('{\"a\":1,\"b\":[2,3]}','$.b[1]')",
        "SELECT json_array(1,'a',null,2.5)",
        "SELECT json_type('[1,2]'), json_type('{}'), json_type('null'), json_type('1.5')",
        "SELECT json_valid('{a:1}'), json_valid('{\"a\":1}')",
        "SELECT json_quote(3.14), json_quote('a\"b')",
        "SELECT json('  {  \"x\" : 1 }  ')",
        "SELECT json_object('a',1,'b',json('[1,2]'))",
        "SELECT '[1,2,3]'->>'$[1]', '{\"a\":5}'->'$.a'",
    ]);
}

#[test]
fn window_functions() {
    assert_match(&[
        "SELECT x, sum(x) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
         FROM (SELECT 1 x UNION SELECT 2 UNION SELECT 3)",
        "SELECT x, lag(x,1,-1) OVER (ORDER BY x) FROM (SELECT 1 x UNION SELECT 2)",
        "SELECT x, ntile(2) OVER (ORDER BY x) FROM (SELECT 1 x UNION SELECT 2 UNION SELECT 3)",
        "SELECT x, percent_rank() OVER (ORDER BY x) \
         FROM (SELECT 1 x UNION SELECT 2 UNION SELECT 3)",
    ]);
}
