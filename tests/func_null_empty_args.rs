//! Two function edge cases where a NULL or empty argument must still produce a
//! non-NULL result, matching SQLite:
//!
//!  * `zeroblob(NULL)` is an empty blob, not NULL — SQLite reads the length via
//!    `sqlite3_value_int64`, which maps NULL to 0.
//!  * `printf('%c', '')` (and `printf('%c', NULL)`) emits a single NUL byte —
//!    SQLite's `%c` defaults `buf[0]` to 0 when the argument has no first
//!    character; a precision repeats it (`%.3c` of `''` is three NUL bytes).
//!
//! Verified byte-for-byte against the sqlite3 3.50.4 CLI (via `hex()`/`quote()`
//! so an embedded NUL cannot be masked by C-string truncation).

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
fn null_and_empty_argument_edge_cases_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let sql = "SELECT typeof(zeroblob(NULL)), quote(zeroblob(NULL)), \
               typeof(zeroblob('')), quote(zeroblob('abc')), \
               hex(printf('%c','')), hex(printf('%c',NULL)), \
               hex(printf('%.3c','')), length(printf('%c','')), \
               hex(printf('%c','abc')), hex(printf('%c',104));";
    assert_eq!(out("sqlite3", sql), out(g, sql));
}
