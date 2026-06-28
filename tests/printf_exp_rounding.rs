//! `printf`/`format` scientific conversions (`%e`/`%E`) round the mantissa
//! **half away from zero**, like SQLite (and like graphite's own `%f`), not
//! half-to-even the way Rust's built-in float formatter does. The two differ
//! only on an exact tie at the rounding position: `printf('%.3e', 1234.5)` is
//! `1.235e+03` (the `5` rounds the `4` up), not `1.234e+03`; `printf('%.0e',
//! 2.5)` is `3e+00`, not `2e+00`. graphite previously delegated the rounding to
//! Rust and produced the half-to-even answer for these ties.
//!
//! The tie-break reads an exact decimal digit of the f64, so a value that only
//! *looks* like a tie but is not exactly representable still follows its true
//! value (`%.2e` of 1.005 stays `1.00e+00` because 1.005's f64 is just below).
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s.trim_end().to_string()
}

#[test]
fn exp_mantissa_rounds_half_away_from_zero() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let mut cases: Vec<String> = Vec::new();
    // A spread of values (exact ties, near-ties, negatives, large/small
    // magnitudes, exponent carries) across precisions 0..=6.
    let values = [
        "1234.5",
        "-1234.5",
        "2.5",
        "-2.5",
        "1.25",
        "1.35",
        "25.0",
        "250.0",
        "125.0",
        "45000.0",
        "9.999",
        "9.9995",
        "0.00012345",
        "1.005",
        "0.0",
        "12345.0",
        "99.5",
        "9.5",
        "999.5",
        "1.235",
        "3.14159",
        "2.5e18",
        "1e20",
        "5e-300",
        "1e300",
    ];
    for v in values {
        for p in 0..=6 {
            cases.push(format!("SELECT printf('%.{p}e', {v})"));
            cases.push(format!("SELECT printf('%.{p}E', {v})"));
        }
    }
    // Default precision, sign flags, and the alternate `#` flag.
    cases.push("SELECT printf('%e', 1234.5)".into());
    cases.push("SELECT printf('%+.3e', 1234.5)".into());
    cases.push("SELECT printf('% .3e', 1234.5)".into());
    cases.push("SELECT printf('%.0e', 9.5)".into());
    cases.push("SELECT printf('%.10e', 3.14159)".into());

    for sql in &cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
