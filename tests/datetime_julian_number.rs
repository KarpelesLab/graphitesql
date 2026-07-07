//! A bare numeric argument to a date/time function is a Julian Day number. Two
//! bugs, both around SQLite's `rawS`/`isError` handling:
//!
//! 1. A modifier that snaps the date (`start of month`/`year`/`day`, `weekday N`)
//!    dropped `valid_jd` and rebuilt from Y/M/D, but the stale `raw_s` flag made
//!    `compute_jd` invalidate the result to NULL — so `date(2460000.5,'start of
//!    month')` returned '' instead of '2023-02-01'. `%j` (day-of-year) hit the
//!    same path. SQLite's `computeHMS` clears `rawS` and its `start of` modifiers
//!    call `computeYMD_HMS`; graphite now does the same.
//! 2. An out-of-range Julian Day (a Unix timestamp like 1719000000 used without
//!    `unixepoch`, or arithmetic past year 9999) must stay NULL through any
//!    modifier chain. graphite lacked SQLite's sticky `isError` flag, so once a
//!    modifier reset the fields it rebuilt an in-range (wrong) date. A ported
//!    `is_error` (set by `compute_jd`/`compute_ymd` exactly where SQLite calls
//!    `datetimeError`) keeps it NULL.
//!
//! Verified byte-for-byte against the sqlite3 3.50.4 CLI (found by a randomized
//! date/time modifier fuzzer).

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin)
        .arg(":memory:")
        .arg(sql)
        .env("TZ", "UTC")
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn datetime_julian_number_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // in-range Julian Day + snapping modifiers (bug 1)
        "date(2460000.5,'start of month')",
        "date(2460000.5,'start of year')",
        "date(2460000.5,'start of day')",
        "date(2460000.5,'weekday 0')",
        "datetime(2460000.5,'weekday 3','+1 hour')",
        "julianday(2460000.5,'start of month','floor')",
        "date(2460000,'start of month')",
        // %j / %s / %J over a Julian-Day input
        "strftime('%j',2460000.5)",
        "strftime('%j',2460000.5,'+1 day')",
        "strftime('%s',2460000.5,'start of year')",
        "strftime('%J',2460000.5,'start of month')",
        // out-of-range Julian Day (Unix timestamp without `unixepoch`) → NULL (bug 2)
        "datetime(1719000000)",
        "datetime(1719000000,'start of year')",
        "date(1719000000,'-1 month','+1 day')",
        "time(1719000000,'+1 year')",
        "strftime('%j',1719000000,'-90 minutes')",
        "julianday(1719000000,'start of day')",
        // arithmetic past the representable window → NULL
        "julianday('9999-12-31','+1 day')",
        "datetime('9999-12-31','+1 month')",
        "date('9999-12-31 24:00:00')",
        // still-valid unixepoch and boundary Julian Days
        "datetime(1719000000,'unixepoch')",
        "datetime(1719000000,'unixepoch','start of day')",
        "date(0)",
        "date(5373484.4)",
        "date(2440587.5,'unixepoch')",
        // regression: ordinary string dates
        "date('2024-02-29','+1 year')",
        "date('2024-01-31','+1 month')",
        "datetime('2024-06-15 12:30:45','start of month')",
        "strftime('%Y-%W-%w','2024-01-01')",
    ];
    let mut sql = String::new();
    for c in cases {
        sql.push_str(&format!("SELECT quote({c});"));
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}
