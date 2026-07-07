//! A scalar function's *integer* argument is coerced the way SQLite's
//! `sqlite3_value_int64` (`sqlite3VdbeIntValue`) does: text/blob is read as its
//! leading **integer** prefix (`sqlite3Atoi64`, stopping at `.`/`e`/any
//! non-digit), NOT parsed as a full floating-point literal. graphite used its
//! numeric `to_i64` (which honours a decimal point and exponent), so
//! `char('1e3')` wrongly produced code point 1000 instead of 1, `substr(s,'1e3')`
//! offset 1000 instead of 1, `zeroblob('1e2')` 100 bytes instead of 1, and
//! `printf('%d','1e3')` printed 1000 instead of 1. Verified byte-for-byte against
//! the sqlite3 3.50.4 CLI (found by a scalar-function fuzzer). `CAST('1e3' AS
//! INT)` already used the integer-prefix parse and is unchanged.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn hex(bin: &str, expr: &str) -> String {
    let sql = format!("SELECT hex({expr});");
    let o = Command::new(bin)
        .arg(":memory:")
        .arg(&sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn integer_argument_uses_integer_prefix_not_float_parse() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Each expression takes an integer argument from a text/real value that the
    // integer-prefix rule and a float parse would read differently ('1e3' -> 1 vs
    // 1000; '1.5e1' -> 1 vs 15), plus controls that must be unaffected.
    let exprs = [
        // char(): code point
        "char('1e3')",
        "char('2e2')",
        "char('1.5e1')",
        "char('65abc')",
        "char('  66 ')",
        "char(65.9)",
        "char('0x10')",
        // substr(): 1-based offset and length
        "substr('abcdefghij','1e3')",
        "substr('abcdefghij','2e0')",
        "substr('abcdefghij',1,'1e1')",
        "substr('abcdefghij','1.5e0',3)",
        // zeroblob(): byte count
        "zeroblob('1e2')",
        "zeroblob('3.9')",
        "zeroblob('2e1')",
        // printf integer conversions
        "printf('%d','1e3')",
        "printf('%x','1e3')",
        "printf('%o','2e1')",
        "printf('%u','1.5e1')",
        // round(): number of digits
        "round(3.14159,'1e1')",
        "round(3.14159,'2e0')",
        // full-float parse contexts must be UNCHANGED (still 1000)
        "printf('%.2f','1e3')",
        "('1e3'+0)",
        "abs('1e3')",
        "cast('1e3' as int)",
    ];
    for e in exprs {
        assert_eq!(hex("sqlite3", e), hex(g, e), "mismatch for `{e}`");
    }
}
