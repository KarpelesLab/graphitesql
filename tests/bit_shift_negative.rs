//! SQLite's bit-shift operators treat a *negative* shift amount as a shift in the
//! opposite direction by its magnitude, and a right shift is *arithmetic*
//! (sign-extending). So `x << -n` == `x >> n` (arithmetic) and `x >> -n` ==
//! `x << n`. graphite's `<<` with a negative amount did a *logical* right shift on
//! `a as u64` — so `-1 << -2` returned `0x3fffffffffffffff` instead of `-1` — and
//! `<< -N` for `N >= 64` returned `0` even for a negative value (should be `-1`).
//! The shift amount is the integer truncation of its operand (`<< -2.5` → `<< -2`).
//! Verified byte-for-byte against the sqlite3 3.50.4 CLI.

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
fn bit_shift_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let vals = [
        "-1",
        "1",
        "0",
        "255",
        "-8",
        "-1024",
        "9223372036854775807",
        "-9223372036854775808",
        "1024",
        "-2.5",
        "'8'",
    ];
    let shifts = [
        "0", "1", "2", "3", "63", "64", "100", "-1", "-2", "-3", "-63", "-64", "-100", "-2.5",
    ];
    let mut sql = String::new();
    for v in vals {
        for s in shifts {
            sql.push_str(&format!("SELECT {v} << {s}, {v} >> {s};"));
        }
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}
