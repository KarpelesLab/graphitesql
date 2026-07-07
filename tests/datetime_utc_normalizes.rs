//! A `utc` or `localtime` modifier normalizes an out-of-range parsed date/time
//! field, matching SQLite. With no modifier, SQLite keeps such fields verbatim
//! (`datetime('2024-06-15 24:00:00')` stays `24:00:00`, and a day past the end of
//! the month is kept unless it is the sole argument), but the `utc`/`localtime`
//! modifiers run `computeJD` + `clearYMD_HMS_TZ`, which re-derives the fields from
//! the Julian day: `2024-02-30` -> `2024-03-01`, `2024-06-15 24:00:00` -> the next
//! day. graphite treated these modifiers as pure no-ops (it has no timezone
//! database, so the shift itself is a no-op), so it skipped the normalization.
//! Verified byte-for-byte against the sqlite3 3.50.4 CLI (found by a date fuzzer).

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
fn utc_localtime_modifier_normalizes_out_of_range_fields() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // day past end of month
        "SELECT datetime('2024-02-30','utc');",
        "SELECT date('2024-02-30','localtime');",
        "SELECT datetime('2023-02-31','utc');",
        "SELECT date('2024-04-31','utc');",
        // 24:00:00 clock rolls into the next day
        "SELECT datetime('2024-06-15 24:00:00','utc');",
        "SELECT datetime('2024-06-15 24:00:00','localtime');",
        "SELECT time('2024-06-15 24:00:00','utc');",
        // the no-modifier forms must be UNCHANGED (kept verbatim)
        "SELECT datetime('2024-06-15 24:00:00');",
        "SELECT datetime('2024-02-30','subsec');",
        "SELECT datetime('2024-02-30','start of day');",
        // valid values pass through utc untouched
        "SELECT datetime('2024-06-15 12:34:56','utc');",
        "SELECT julianday('2000-01-01','utc');",
        // chained: utc-normalized value then a further modifier
        "SELECT datetime('2024-02-30','utc','+1 day');",
        "SELECT datetime('2024-06-15 24:00:00','utc','start of day');",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "mismatch for `{sql}`");
    }
}
