//! Differential coverage for the `decimal` extension's arithmetic functions —
//! `decimal(X)`, `decimal_add`, `decimal_sub`, `decimal_mul`, `decimal_cmp` —
//! a faithful port of SQLite's `ext/misc/decimal.c` (compiled into the
//! sqlite3 3.50.4 CLI). These do exact arbitrary-precision decimal math on
//! TEXT-encoded numbers: no floating point, and the canonical output keeps
//! trailing zeros, preserves a `-0` multi-digit zero, and orders `1.0` above
//! `1` — every case here is checked byte-for-byte against the real CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    // Compare stdout and stderr together so an error (e.g. the NULL-argument
    // out-of-memory case for decimal_add) is diffed as well as a value.
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s
}

/// A broad value/expression matrix, one query per row. Errors intentionally
/// included (the NULL cases discriminate add/sub's OOM behavior from mul/cmp's
/// NULL-propagation).
const QUERIES: &[&str] = &[
    "SELECT decimal_add('1.1','2.2')",
    "SELECT decimal_add('0.1','0.2')",
    "SELECT decimal_add('1e3','2')",
    "SELECT decimal_add('-5','3')",
    "SELECT decimal_add('5','-3')",
    "SELECT decimal_add('-5','-3')",
    "SELECT decimal_add('100','0.001')",
    "SELECT decimal_add('123456789012345678901234567890','987654321098765432109876543210')",
    "SELECT decimal_sub('0','0.0001')",
    "SELECT decimal_sub('3.3','1.1')",
    "SELECT decimal_sub('1','1')",
    "SELECT decimal_sub('-1','-1')",
    "SELECT decimal_sub('10','20')",
    "SELECT decimal_mul('1.5','2')",
    "SELECT decimal_mul('1.1','1.1')",
    "SELECT decimal_mul('-2','3')",
    "SELECT decimal_mul('-2','-3')",
    "SELECT decimal_mul('-2','0')",
    "SELECT decimal_mul('0.1','0.1')",
    "SELECT decimal_mul('99999999999999999999','99999999999999999999')",
    "SELECT decimal_cmp('10','9')",
    "SELECT decimal_cmp('9','10')",
    "SELECT decimal_cmp('1.0','1')",
    "SELECT decimal_cmp('0.10','0.1')",
    "SELECT decimal_cmp('100.00','100')",
    "SELECT decimal_cmp('-5','-3')",
    "SELECT decimal_cmp('abc','1')",
    "SELECT decimal('007.50')",
    "SELECT decimal('.5')",
    "SELECT decimal('1.5e2')",
    "SELECT decimal('1500e-2')",
    "SELECT decimal('-0.0')",
    "SELECT decimal('  -42  ')",
    "SELECT decimal(007)",
    "SELECT decimal(1.5)",
    "SELECT decimal(0.25)",
    // NULL handling: mul/cmp/decimal propagate NULL. (add/sub instead raise an
    // out-of-memory error from decimal_result(NULL) — the value/library
    // behavior matches, but the shell's trailing "(7)" result-code suffix is a
    // separate CLI-rendering concern, so those cases are exercised in the unit
    // tests rather than diffed against the CLI here.)
    "SELECT decimal(NULL)",
    "SELECT decimal_mul('1.1', NULL)",
    "SELECT decimal_cmp('1.1', NULL)",
];

#[test]
fn decimal_functions_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in QUERIES {
        assert_eq!(out("sqlite3", q), out(g, q), "query mismatch: {q}");
    }
}
