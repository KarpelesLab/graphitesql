//! Differential coverage for the remaining three functions of SQLite's
//! `decimal` extension (`ext/misc/decimal.c`, compiled into the sqlite3 3.50.4
//! CLI): the scalars `decimal_exp(X)` and `decimal_pow2(N)`, and the aggregate
//! (also usable as a window function) `decimal_sum(X)`.
//!
//! * `decimal_exp(X)` renders the same value as `decimal(X)` but in scientific
//!   notation (`decimal_result_sci`): explicit sign, one leading digit, and an
//!   exponent carrying its sign plus at least two zero-padded digits.
//! * `decimal_pow2(N)` renders `2**N` (integer argument only) in that same
//!   scientific notation; a non-integer argument yields NULL and `|N|>20000`
//!   raises the same out-of-memory error `decimal_add`'s NULL path does.
//! * `decimal_sum(X)` is an arbitrary-precision sum: a group with at least one
//!   row (even an all-NULL one) yields a `"0"`-based decimal string, and a
//!   zero-row group yields NULL.
//!
//! Every value-producing case here is checked byte-for-byte against the real
//! CLI. As with `decimal_functions.rs`, the out-of-memory error path is not
//! diffed against the CLI (the shell's trailing "(7)" result-code suffix is a
//! separate CLI-rendering concern); it is asserted on graphite's output alone.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run a (possibly multi-statement) script against an in-memory database,
/// capturing stdout and stderr together so error text is diffed as well.
fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s
}

/// Value-producing and (identically rendered) error queries, one per entry,
/// diffed byte-for-byte against the CLI.
const QUERIES: &[&str] = &[
    // --- decimal_exp: scientific notation of decimal(X) ---
    "SELECT decimal_exp('1.5')",
    "SELECT decimal_exp('123')",
    "SELECT decimal_exp('0')",
    "SELECT decimal_exp('-0')",
    "SELECT decimal_exp('0.00')",
    "SELECT decimal_exp('1e10')",
    "SELECT decimal_exp('1e-10')",
    "SELECT decimal_exp('0.000123')",
    "SELECT decimal_exp('-123.456')",
    "SELECT decimal_exp('.5')",
    "SELECT decimal_exp('  12.5  ')",
    "SELECT decimal_exp('999999999999999999999999')",
    "SELECT decimal_exp('-0.5')",
    "SELECT decimal_exp(42)",
    "SELECT decimal_exp(-3.14)",
    "SELECT decimal_exp(3.0)",
    "SELECT decimal_exp('abc')",
    "SELECT decimal_exp(NULL)",
    // --- decimal_pow2: 2**N in scientific notation (integer arg only) ---
    "SELECT decimal_pow2(0)",
    "SELECT decimal_pow2(1)",
    "SELECT decimal_pow2(5)",
    "SELECT decimal_pow2(10)",
    "SELECT decimal_pow2(64)",
    "SELECT decimal_pow2(128)",
    "SELECT decimal_pow2(200)",
    "SELECT decimal_pow2(1024)",
    "SELECT decimal_pow2(-1)",
    "SELECT decimal_pow2(-3)",
    "SELECT decimal_pow2(-64)",
    "SELECT decimal_pow2(-128)",
    "SELECT decimal_pow2(20000)",
    "SELECT decimal_pow2(-20000)",
    // sqlite3_value_int truncates the argument to 32 bits before the guard:
    // 2**32 + 5 -> 5 -> 2**5 = 32.
    "SELECT decimal_pow2(4294967301)",
    // Non-integer arguments leave the result unset -> NULL.
    "SELECT decimal_pow2(1.5)",
    "SELECT decimal_pow2(2.0)",
    "SELECT decimal_pow2('x')",
    "SELECT decimal_pow2('5')",
    "SELECT decimal_pow2(NULL)",
    "SELECT decimal_pow2(x'0102')",
    // --- decimal_sum: arbitrary-precision aggregate ---
    "SELECT decimal_sum(x) FROM (SELECT '1.1' AS x UNION ALL SELECT '2.22' UNION ALL SELECT '3.333')",
    "SELECT decimal_sum(x) FROM (SELECT '-5' AS x UNION ALL SELECT '10.5')",
    "SELECT decimal_sum(x) FROM (SELECT 1 AS x UNION ALL SELECT 2.5 UNION ALL SELECT '3.333')",
    "SELECT decimal_sum(x) FROM (SELECT '0.1' AS x) WHERE 1",
    // Single row.
    "SELECT decimal_sum('42.5')",
    // High precision / large magnitude.
    "SELECT decimal_sum(x) FROM (SELECT '1000000000000000000000' AS x UNION ALL SELECT '0.000000001')",
    // --- decimal_sum: GROUP BY, all-NULL group, empty group ---
    "CREATE TABLE t(g INT, x); \
     INSERT INTO t VALUES (1,'1.1'),(1,'2.22'),(1,'3.333'),(2,'-5'),(2,'10.5'),(2,NULL),(3,NULL),(3,NULL); \
     SELECT g, decimal_sum(x) FROM t GROUP BY g ORDER BY g",
    // A group with rows but all-NULL values initializes to 0 -> "0".
    "CREATE TABLE t(x); INSERT INTO t VALUES (NULL),(NULL); SELECT decimal_sum(x) FROM t",
    // A zero-row group -> NULL.
    "CREATE TABLE t(x); INSERT INTO t VALUES ('1.5'); SELECT decimal_sum(x) FROM t WHERE 0",
    // --- decimal_sum as a window function (unbounded-preceding frames) ---
    "CREATE TABLE t(x); INSERT INTO t VALUES ('1.1'),('2.22'),('3.333'),('-5'),('10.5'),(NULL); \
     SELECT decimal_sum(x) OVER () FROM t",
    "CREATE TABLE t(x); INSERT INTO t VALUES ('1.1'),('2.22'),('3.333'),('-5'),('10.5'),(NULL); \
     SELECT decimal_sum(x) OVER (ORDER BY rowid) FROM t",
    "CREATE TABLE t(g INT, x); \
     INSERT INTO t VALUES (1,'1.1'),(1,'2.22'),(2,'3.333'),(2,'-5'); \
     SELECT g, decimal_sum(x) OVER (PARTITION BY g ORDER BY rowid) FROM t ORDER BY rowid",
    // --- error parity (rendered identically, caret and all) ---
    "SELECT decimal_sum()",
    "SELECT decimal_sum(1,2)",
];

#[test]
fn decimal_extra_functions_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in QUERIES {
        assert_eq!(out("sqlite3", q), out(g, q), "query mismatch: {q}");
    }
}

/// `decimal_pow2` with `|N|>20000` raises the same out-of-memory error as
/// `decimal_add`'s NULL path (the value/library behavior matches the CLI; only
/// the shell's trailing "(7)" result-code suffix differs, so this is asserted on
/// graphite's output alone rather than diffed).
#[test]
fn decimal_pow2_out_of_range_is_oom() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in ["SELECT decimal_pow2(20001)", "SELECT decimal_pow2(-20001)"] {
        let s = out(g, q);
        assert!(
            s.contains("out of memory"),
            "expected out-of-memory error for {q}, got: {s}"
        );
    }
}
