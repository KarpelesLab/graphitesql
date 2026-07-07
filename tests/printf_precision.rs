//! printf/format precision-handling parity with SQLite, from a randomized
//! specifier fuzzer. `%c` with a precision repeats the character that many times
//! (`%.5c` of 'A' is "AAAAA"), min once. `%q`/`%Q`/`%w` apply precision to the
//! *input* characters (escaping is applied after, so `'` still doubles); `%Q`
//! always wraps in quotes, uncounted (`%.0Q` of 'hi' is `''`). The `#` alt flag
//! forces a decimal point on `%e`/`%E` even at precision 0 (`%#.0e` of 42 is
//! "4.e+01"). A `.*` precision uses the *magnitude* of a negative argument
//! (`%.*f` with -2 is `%.2f`), not a clamp to 0. The fractional precision of the
//! float conversions is capped at 1e8 (as in SQLite), so a `%.999999999f` no
//! longer hangs producing ~1e9 digits. Verified byte-for-byte against the
//! sqlite3 3.50.4 CLI.

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
fn printf_precision_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // %c repeat
        "printf('%.5c','A')",
        "printf('%.3c',65)",
        "printf('%.0c','A')",
        "printf('%5.3c','A')",
        "printf('%-10.10c',7)",
        // %q/%Q/%w input precision
        "printf('%.2q','hello')",
        "printf('%.2Q','hello')",
        "printf('%10.2Q','hi')",
        "printf('%.3q','a''bc')",
        "printf('%.3Q','a''bc')",
        "printf('%.0Q','hi')",
        "printf('%.3w','a\"bc')",
        "printf('%.5Q','he')",
        // # alt flag forces the point on %e/%E at precision 0
        "printf('%#.0e',42)",
        "printf('%#.0E',42)",
        "printf('%#.0e',0.0)",
        "printf('%#.2e',42)",
        // negative .* precision uses the magnitude
        "printf('%.*s',-1,'abcde')",
        "printf('%.*f',-2,1.5)",
        "printf('%.*Q',-1,255)",
        "printf('%.*e',-1,2.5)",
        "printf('%.*g',-2,3.14159)",
        "printf('%.*d',-3,42)",
    ];
    let mut sql = String::new();
    for c in cases {
        sql.push_str(&format!("SELECT quote({c});"));
    }
    // The float-precision cap: a huge precision must not hang, and its length
    // matches SQLite's 1e8 cap. Compare only the length (the value is 100 MB).
    sql.push_str("SELECT length(printf('%.999999999f',1.0));");
    sql.push_str("SELECT length(printf('%.500000000e',2.5));");
    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}
