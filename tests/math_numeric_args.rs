//! The math functions (`sqrt`/`sin`/`cos`/`tan`/`exp`/`ln`/`log`/`log10`/`pow`/
//! `atan2`/`mod`/`degrees`/`radians`/…) classify each argument with SQLite's
//! `sqlite3_value_numeric_type`: an INTEGER or REAL — or a text that is *wholly*
//! numeric (`'4'`, `'  9  '`, `'2e2'`) — is fed to the function, while a
//! non-numeric text, a blob, or NULL yields NULL. graphite routed every argument
//! through the lax `to_f64`, which turns `'abc'` or a blob into `0.0` — so
//! `sqrt('abc')` returned `0.0` (sqlite: NULL), `tan(x'34')` returned a number
//! (sqlite: NULL), and `pow('x',2)` returned `0.0` (sqlite: NULL). Verified
//! byte-for-byte (value and `typeof`) against the sqlite3 3.50.4 CLI.

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
fn math_functions_numeric_type_args_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");

    // Argument classes: integer, real, numeric text (int / real / scientific /
    // padded), non-numeric text, empty text, blob, NULL.
    let args = [
        "4", "4.5", "'4'", "'4.5'", "'2e2'", "'  9  '", "'abc'", "''", "x'34'", "x'3132'", "NULL",
        "-1", "0",
    ];
    // `exp`/`sinh`/`cosh` are excluded: their values are byte-exact for small
    // arguments but diverge in the last ULP once they overflow toward `e^200`
    // (the known transcendental precision residual — irreducible without libm),
    // which is orthogonal to the numeric-type classification under test here.
    let unary = [
        "sqrt", "sin", "cos", "tan", "asin", "acos", "atan", "tanh", "ln", "log10", "log2",
        "degrees", "radians",
    ];
    let mut sql = String::new();
    for f in unary {
        for a in args {
            sql.push_str(&format!("SELECT typeof({f}({a})),{f}({a});"));
        }
    }
    // Two-argument functions, mixing a non-numeric arg into each position.
    for f in ["pow", "atan2", "mod", "log"] {
        for a in ["'x'", "2", "'2'", "x'34'"] {
            for b in ["3", "'3'", "'y'"] {
                sql.push_str(&format!("SELECT typeof({f}({a},{b})),{f}({a},{b});"));
            }
        }
    }
    // Single-argument `log` (base-10).
    for a in args {
        sql.push_str(&format!("SELECT typeof(log({a})),log({a});"));
    }

    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}
