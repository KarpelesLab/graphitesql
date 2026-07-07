//! SQLite's date/time functions accept a bare `HH:MM[:SS[.FFF]]` modifier (with
//! an optional leading `+`/`-`) that adds a signed time-of-day to the value.
//! graphite's `apply_time_shift` required a leading sign, so an unsigned `'12:00'`
//! wrongly returned NULL; it also missed SQLite's whole-day drop, so `'24:00'`
//! added a day instead of nothing and `'24:30'` should add 30 minutes. Verified
//! byte-for-byte against the sqlite3 3.50.4 CLI (found by a date-modifier fuzzer).

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin)
        .env("TZ", "UTC")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn hh_mm_time_shift_modifier_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "2024-06-15 06:30:15";
    let mods = [
        "12:00",      // unsigned -> positive shift (was wrongly NULL)
        "+12:00",     // explicit +
        "-03:00",     // explicit -
        "01:30",      // unsigned, rolls the clock
        "23:59",      // rolls to the next day
        "24:00",      // whole day dropped -> no change
        "24:30",      // whole day dropped -> +30 minutes
        "24:59",      // -> +59 minutes
        "00:00:00.5", // fractional seconds
        "23:59:59",   // seconds
        "-00:00:01",  // -1 second, crosses midnight backwards
        "+23:59:59.999",
        // invalid shapes must stay NULL on both engines
        "25:00",
        "12:60",
        "1:00",
        "01:5",
        "12:00:60",
    ];
    // Cover every function so the modifier is exercised through each entry point.
    for f in ["datetime", "time", "date", "julianday", "unixepoch"] {
        for m in mods {
            let sql = format!("SELECT {f}('{base}','{m}');");
            assert_eq!(out("sqlite3", &sql), out(g, &sql), "mismatch for `{sql}`");
        }
    }
    // Chained with other modifiers, and applied to a date-only value.
    let chained = [
        "SELECT datetime('2024-06-15','12:00','+1 day');",
        "SELECT datetime('2024-06-15 20:00:00','08:00');",
        "SELECT time('2024-06-15 12:00:00-05:00','12:00');",
        "SELECT strftime('%Y-%m-%d %H:%M:%S','2024-02-29 13:45:00','12:30');",
    ];
    for sql in chained {
        assert_eq!(out("sqlite3", sql), out(g, sql), "mismatch for `{sql}`");
    }
}
