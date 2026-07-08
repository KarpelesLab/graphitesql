//! `quote()` of a REAL renders it at round-trip precision, matching SQLite:
//! `%!0.15g`, falling back to `%!0.20e` when the 15-significant-digit form does
//! not reparse to the same `double`. graphite previously reused its 15-digit
//! column rendering (`format_real`), so `quote()` of most computed reals was
//! lossy — e.g. `quote(2.0/3.0)` gave `0.666666666666667` (which reparses to a
//! *different* double) instead of `6.666666666666666296e-01`. A `.dump` of such
//! a value therefore did not round-trip. Now ported from SQLite's
//! `sqlite3FpDecode`. Verified byte-for-byte against the sqlite3 3.50.4 CLI.
//!
//! NOTE: SQLite's decoder is byte-exact only for realistic magnitudes here; at
//! extreme exponents (|exp| beyond ~1e83) the CLI's last digits depend on its C
//! compiler's floating-point code generation (a faithful C port of the same
//! source produces different last digits), so those are excluded — see the
//! `datetime`/math ULP residual precedent.

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
fn quote_real_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // A spread of realistic reals: nice literals (15g path) and computed values
    // that need the 20e fallback, plus signs, small/large magnitudes, and the
    // rendering of an infinity.
    let exprs = [
        "0.1",
        "1.5",
        "100.0",
        "-2.5",
        "0.0",
        "2.0/3.0",
        "1.0/3.0",
        "1.0/7.0",
        "22.0/7.0",
        "3.141592653589793",
        "2.718281828459045",
        "355.0/113.0",
        "123.456",
        "9.87654321e-5",
        "1.23456789012345e20",
        "9.87e-15",
        "6.022e23",
        "1.602e-19",
        "cast(9223372036854775807 as real)",
        "cast(-9223372036854775808 as real)",
        "1e100",
        "1e-100",
        "2.5e30",
        "-4.2e-30",
        "9e999",
        "-9e999", // infinities -> ±9.0e+999
        "0.30000000000000004",
        "1234567890123456.0",
    ];
    let mut sql = String::new();
    for e in exprs {
        sql.push_str(&format!("SELECT quote(CAST(({e}) AS REAL));"));
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));

    // Round-trip property through a table: store a computed real, dump its quoted
    // form, and confirm graphite == sqlite for each.
    let tbl = "CREATE TABLE t(x REAL);\
        INSERT INTO t VALUES(2.0/3.0),(1.0/7.0),(355.0/113.0),(0.1+0.2),(1.0/49.0);\
        SELECT quote(x) FROM t ORDER BY rowid;";
    assert_eq!(out("sqlite3", tbl), out(g, tbl));
}
