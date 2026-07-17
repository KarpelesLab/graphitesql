//! Corruption-robustness ("fuzz-style") tests for the SQL front end (roadmap §6).
//!
//! Feed a large, systematically-generated set of malformed / truncated / edge
//! SQL strings to the parser (via `Connection::query`) and assert each returns
//! `Err` and, crucially, **never panics or overflows the stack**. The deep-
//! nesting cases specifically guard the recursive-descent parser's depth limit:
//! before that limit existed, thousands of `(` or `CASE WHEN` aborted the
//! process with a stack overflow.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Run `sql` and assert it does not panic. Returns whether it parsed/ran `Ok`.
fn run_no_panic(c: &Connection, sql: &str, tag: &str) -> bool {
    let r = catch_unwind(AssertUnwindSafe(|| c.query(sql).is_ok()));
    match r {
        Ok(ok) => ok,
        Err(_) => panic!("parser PANICKED on malformed SQL (tag={tag}, sql={sql:?})"),
    }
}

/// Assert `sql` is rejected with an `Err` and never panics.
fn must_err(c: &Connection, sql: &str, tag: &str) {
    assert!(
        !run_no_panic(c, sql, tag),
        "expected an error for {tag}: {sql:?}"
    );
}

#[test]
fn malformed_sql_strings_error_not_panic() {
    let c = Connection::open_memory().unwrap();
    c.query("CREATE TABLE t(a, b, c)").ok();

    // Clearly malformed: unterminated literals/comments, unbalanced parens,
    // dangling operators, truncated clauses. Each MUST be rejected.
    let bad = [
        "SELECT",
        "SELECT * FROM",
        "FROM t",
        "WHERE",
        "(",
        ")",
        "()",
        "SELECT (",
        "SELECT )",
        "SELECT 'unterminated",
        "SELECT \"unterminated",
        "SELECT [unterminated",
        "SELECT x'deadbeef",
        "SELECT x'gg'",
        "SELECT x'",
        "/* unterminated comment",
        "SELECT 1.2.3",
        "SELECT 1e",
        "SELECT .e",
        "SELECT 1 +",
        "SELECT + ",
        "SELECT * FROM t WHERE",
        "SELECT * FROM t GROUP BY",
        "SELECT * FROM t ORDER BY",
        "SELECT * FROM t LIMIT",
        "INSERT INTO",
        "INSERT INTO t VALUES",
        "INSERT INTO t VALUES (",
        "UPDATE",
        "DELETE FROM",
        "CREATE TABLE",
        "CREATE TABLE t(",
        "CREATE INDEX",
        "SELECT CAST",
        "SELECT CAST(1 AS",
        "SELECT \\",
        "SELECT 'a''b",
        "WITH",
        "WITH x AS",
        "SELECT * FROM (SELECT",
        "SELECT COUNT(",
        "SELECT 1 BETWEEN",
        "SELECT 1 IN",
        "SELECT 1 IN (",
        "SELECT CASE",
        "SELECT CASE WHEN",
        "SELECT 1 COLLATE",
        "PRAGMA",
        "EXPLAIN",
        "EXPLAIN QUERY PLAN",
        "SAVEPOINT",
        "ATTACH",
        "DETACH",
    ];
    for (i, s) in bad.iter().enumerate() {
        must_err(&c, s, &format!("bad-{i}"));
    }

    // Edge cases that may legitimately parse or error depending on context; we
    // only require they not panic (some are valid, e.g. `SELECT *`).
    let edge = [
        "",
        " ",
        ";",
        ";;;",
        "SELECT ",
        "SELECT *",
        "SELECT `unterminated",
        "SELECT 0x",
        "SELECT 0xZZ",
        "SELECT 1e999999999",
        "SELECT 99999999999999999999999999999999999999",
        "SELECT -99999999999999999999999999999999999999",
        "SELECT $",
        "SELECT :",
        "SELECT @",
        "SELECT #",
        "SELECT \0",
        "\u{0}\u{1}\u{2}",
        "VALUES",
        "BEGIN BEGIN",
        // an unterminated block comment is whitespace (tokenize.c CC_SLASH),
        // so this is a complete `SELECT 1` — valid, like sqlite
        "SELECT 1 /* nested",
    ];
    for (i, s) in edge.iter().enumerate() {
        run_no_panic(&c, s, &format!("edge-{i}"));
    }
}

#[test]
fn deeply_nested_input_is_rejected_not_overflowed() {
    let c = Connection::open_memory().unwrap();
    // Depths far beyond the parser's recursion cap. Each MUST be rejected with an
    // `Err` rather than overflowing the (small, default) test-thread stack.
    for depth in [50usize, 200, 1000, 5000, 20000] {
        must_err(
            &c,
            &format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth)),
            &format!("paren-{depth}"),
        );
        must_err(
            &c,
            &format!("SELECT {}1", "NOT ".repeat(depth)),
            &format!("not-{depth}"),
        );
        must_err(
            &c,
            &format!("SELECT {}1", "-".repeat(depth)),
            &format!("neg-{depth}"),
        );
        must_err(
            &c,
            &format!("SELECT {}1{}", "abs(".repeat(depth), ")".repeat(depth)),
            &format!("func-{depth}"),
        );
        must_err(
            &c,
            &format!("SELECT {}1 END", "CASE WHEN 1 THEN ".repeat(depth)),
            &format!("case-{depth}"),
        );
        // Deeply nested sub-selects (the heaviest path for the later phases).
        let mut sub = String::from("SELECT 1");
        for _ in 0..depth {
            sub = format!("SELECT * FROM ({sub})");
        }
        must_err(&c, &sub, &format!("subq-{depth}"));
    }
}

#[test]
fn truncations_and_byte_soup_never_panic() {
    let c = Connection::open_memory().unwrap();
    c.query("CREATE TABLE t(a, b, c)").ok();

    // Truncate a long valid statement at every byte boundary.
    let long = "SELECT a, b, c, COUNT(*), SUM(a+b*c) FROM t \
        WHERE a > 1 AND b IN (1,2,3) GROUP BY a HAVING COUNT(*) > 0 \
        ORDER BY a DESC LIMIT 10 OFFSET 5";
    for i in 0..long.len() {
        run_no_panic(&c, &long[..i], &format!("trunc-{i}"));
    }

    // Deterministic ASCII soup (printable range plus SQL punctuation).
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    for trial in 0..3000 {
        let len = (state as usize % 80) + 1;
        let mut s = String::new();
        for _ in 0..len {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let ch = (b' ' as u64 + (state >> 40) % 95) as u8 as char;
            s.push(ch);
        }
        run_no_panic(&c, &s, &format!("rand-{trial}"));
    }

    // Multibyte/UTF-8 input truncated at every char boundary.
    let uni = "SELECT 'café résumé 日本語 emoji 😀 more'";
    for i in 0..uni.len() {
        if uni.is_char_boundary(i) {
            run_no_panic(&c, &uni[..i], &format!("uni-{i}"));
        }
    }
}
