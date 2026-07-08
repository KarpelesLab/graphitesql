//! The `printf` `!` (alt-form-2) flag renders a float through the port of
//! SQLite's `sqlite3FpDecode` (`util::fpdecode`), lifting the 16-significant-digit
//! cap to SQLite's `mxRound = 26` — so `%!.20f` of `0.1` is
//! `0.1000000000000000055`, not C's exact-f64 `0.10000000000000000555`. graphite
//! previously emitted the exact f64 expansion here (a documented divergence).
//! Verified byte-for-byte against the sqlite3 3.50.4 CLI. Extreme magnitudes
//! (|exp| beyond ~1e80) are excluded: the CLI's last digits there depend on its C
//! compiler's floating-point codegen, not the shared algorithm.

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
fn bang_high_precision_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let exprs = [
        // The classic residual cases, now matched.
        "printf('%!.20f', 0.1)",
        "printf('%!.25f', 0.1)",
        "printf('%!.25f', 3.14159)",
        "printf('%!.20e', 0.1)",
        "printf('%!.20g', 1.0/3.0)",
        "printf('%!.30f', 0.2)",
        // Signs, the +/space/# flags, width, and %E/%G uppercase.
        "printf('%!+.20f', -0.1)",
        "printf('%! .18e', 2.0/3.0)",
        "printf('%!#.0f', 42.0)",
        "printf('%!25.20g', 123.456)",
        "printf('%!.20E', 6.022e23)",
        "printf('%!.18G', 0.000123456)",
        // Whole numbers, zero, negative zero.
        "printf('%!.20f', 100.0)",
        "printf('%!.15g', 0.0)",
        "printf('%!.15g', -0.0)",
        "printf('%!g', 1000000.0)",
        // Non-finite.
        "printf('%!f', 1e400)",
        "printf('%!e', -1e400)",
    ];
    let mut sql = String::new();
    for e in exprs {
        sql.push_str(&format!("SELECT {e};"));
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}
