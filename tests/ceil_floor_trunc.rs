//! `ceil` / `ceiling` / `floor` / `trunc` dispatch on SQLite's
//! `sqlite3_value_numeric_type`: an INTEGER argument is returned UNCHANGED (its
//! exact value — including i64 extremes — survives, so `typeof` stays `integer`),
//! a REAL is rounded to a REAL, and a non-numeric text / blob / NULL argument
//! yields NULL. graphite used to coerce every argument to a real (returning `1.0`
//! for `ceil(1)`, losing precision on large integers, and `0.0` for `ceil('abc')`).
//! Verified byte-for-byte (value and `typeof`) against the sqlite3 3.50.4 CLI.

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
fn ceil_floor_trunc_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let args = [
        "1",
        "0",
        "-1",
        "2.5",
        "-2.5",
        "3.0",
        "'10'",
        "'10.5'",
        "'10.0'",
        "'2e2'",
        "'  7  '",
        "'abc'",
        "''",
        "NULL",
        "9223372036854775807",
        "-9223372036854775808",
        "1e300",
        "x'3132'",
        "'-0.0'",
    ];
    let mut sql = String::new();
    for fn_ in ["ceil", "ceiling", "floor", "trunc"] {
        for a in args {
            sql.push_str(&format!("SELECT typeof({fn_}({a})),{fn_}({a});"));
        }
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}
