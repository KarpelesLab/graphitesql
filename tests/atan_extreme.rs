//! `atan(x)` for a very large `|x|` is within half a ULP of ±π/2 and rounds to
//! exactly ±π/2. graphite's half-angle reduction squared the argument, which
//! overflows to +∞ for `|x| ≳ 1.3e154` — collapsing the reduction and returning
//! `0.0` (sqlite: ±π/2). `atan2`, which delegates to `atan`, was wrong the same
//! way. Verified byte-for-byte against the sqlite3 3.50.4 CLI.

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
fn atan_extreme_args_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let xs = [
        "1e154", "1.3e154", "2e154", "1e155", "1e200", "1e300", "1e308",
        "-1e154", "-2e154", "-1e300", "-1e308", "9223372036854775807",
        "-9223372036854775808", "1e16", "1e100", "1.5", "-1.5", "0.0",
    ];
    let mut sql = String::new();
    for x in xs {
        sql.push_str(&format!("SELECT atan({x});"));
        sql.push_str(&format!("SELECT atan2({x},1);"));
        sql.push_str(&format!("SELECT atan2(1,{x});"));
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}
