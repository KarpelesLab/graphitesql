//! Ordinary (non-`!`) `printf` `%f`/`%e`/`%g` conversions render through the
//! `sqlite3FpDecode` port (`printf.c`'s `etFLOAT`/`etEXP`/`etGENERIC` arm), not
//! a separate renderer: 16-significant-digit decode, `#`-flag decimal point,
//! `%g` trailing-zero trim, inline `,` grouping (commas woven between the
//! pre-decimal digits, plain zeros for the `0` flag), and the `0` flag's
//! numeric rendering of an infinity (the digit 9 with the decimal point at
//! position 1000). Verified byte-for-byte against the sqlite3 3.50.4 CLI.

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
fn printf_floats_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let vals = [
        "0.0",
        "-0.0",
        "2.0",
        "0.1",
        "2.0/3.0",
        "1e20",
        "1e-20",
        "-1234.5678",
        "9.995",
        "1e-5",
        "5e-324",
        "1.7976931348623157e308",
        "123456789012345.678",
        "100000.0",
        "1000000.0",
    ];
    let fmts = [
        "%f", "%e", "%g", "%E", "%G", "%.0f", "%.3f", "%.1e", "%.0e", "%.0g", "%.17g", "%.20f",
        "%.20e", "%#f", "%#.0f", "%#.0e", "%#g", "%#.3g", "%!f", "%!e", "%!g", "%+.3e", "% g",
        "%12.3f", "%-12.3e", "%012.3f", "%,f", "%,.2f", "%,.10g",
    ];
    let mut sql = String::new();
    for v in vals {
        for f in fmts {
            sql.push_str(&format!("SELECT quote(printf('{f}|',{v}));"));
        }
    }
    // Non-finite: Inf is text unless the `0` flag renders it numerically.
    for c in [
        "printf('%f|%e|%g',1e999,1e999,1e999)",
        "printf('%f|%e|%g',-1e999,-1e999,-1e999)",
        "printf('%+f|% e',1e999,1e999)",
        "printf('%010e',1e999)",
        "printf('%010e',-1e999)",
        "printf('%010g',1e999)",
        "printf('%!010g',1e999)",
        "length(printf('%010f',1e999))",
        "substr(printf('%010f',1e999),1,3)",
    ] {
        sql.push_str(&format!("SELECT quote({c});"));
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}
