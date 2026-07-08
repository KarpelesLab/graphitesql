//! SQLite's `sum`/`avg`/`total` keep an exact integer running sum until the
//! first non-integer input (or an integer overflow), then switch to a
//! Kahan–Babuška–Neumaier compensated double sum seeded from that integer sum
//! (`sumStep`). graphite summed naively, so a large cancellation dropped small
//! contributions (`1e308 + 3 − 1e308` came out `0`, and mixing `INT64` extremes
//! with a real lost the exact integer part). Verified against the sqlite3 3.50.4
//! CLI. Extreme-exponent *quote rendering* (`|exp| ≳ 300`) is a separate
//! floating-point artifact and is avoided here.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn compensated_and_exact_integer_sum() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Compensated summation keeps the small contribution through cancellation.
        "CREATE TABLE t(x);INSERT INTO t VALUES(1e308),(3),(-1e308);\
         SELECT sum(x),total(x),avg(x) FROM t;",
        // INT64 extremes cancel exactly (-1), then a real adds on top: 1.5, avg 0.3.
        "CREATE TABLE t(x);\
         INSERT INTO t VALUES(-1),(1),(9223372036854775807),(-9223372036854775808),('2.5');\
         SELECT quote(sum(x)),quote(avg(x)) FROM t;",
        "CREATE TABLE t(x);\
         INSERT INTO t VALUES(9223372036854775807),(-9223372036854775808),(0.5);\
         SELECT quote(total(x)) FROM t;",
        // A real input after an integer overflow rescues sum() from erroring.
        "CREATE TABLE t(x);\
         INSERT INTO t VALUES(9223372036854775807),(9223372036854775807),(1.5);\
         SELECT quote(sum(x)) FROM t;",
        // A pure all-integer overflow still errors — check both engines reject it.
        // (Handled separately below since it produces no rows.)
        // Plain integer sum stays an integer.
        "CREATE TABLE t(x);INSERT INTO t VALUES(1),(2),(3);\
         SELECT sum(x),typeof(sum(x)),total(x),avg(x) FROM t;",
        // Mixed integer text and reals.
        "CREATE TABLE t(x);INSERT INTO t VALUES('5'),(2.5),(3),('abc'),(NULL);\
         SELECT quote(sum(x)),quote(avg(x)),quote(total(x)) FROM t;",
        // Empty / all-NULL: sum & avg are NULL, total is 0.0.
        "CREATE TABLE t(x);INSERT INTO t VALUES(NULL),(NULL);\
         SELECT quote(sum(x)),quote(avg(x)),quote(total(x)) FROM t;",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for `{sql}`");
    }
    // Pure all-integer overflow: both must raise "integer overflow".
    let ov =
        "CREATE TABLE t(x);INSERT INTO t VALUES(9223372036854775807),(1);SELECT sum(x) FROM t;";
    assert!(out("sqlite3", ov).is_empty());
    let e = Command::new(g).arg(":memory:").arg(ov).output().unwrap();
    assert!(
        String::from_utf8_lossy(&e.stderr).contains("integer overflow"),
        "graphite should report integer overflow"
    );
}
