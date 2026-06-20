//! Differential testing of REAL value formatting, which the main corpus avoids
//! (it uses only int/text). graphitesql must print reals exactly like the
//! installed sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

#[test]
fn real_formatting_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let exprs = [
        "1.0/3",
        "2.0/3",
        "1e20",
        "1e-10",
        "0.0001",
        "123456789012345.0",
        "1234567890123456789.0",
        "0.1+0.2",
        "1.5e300",
        "-0.0",
        "100000000000000000.0",
        "3.14159265358979",
        "1.0/7",
        "2.5",
        "1000000.0",
        "0.000001",
        "1e16",
        "1e15",
        "10.0/4",
        "22.0/7",
        "1.0/3.0*3.0",
        "0.5",
        "-2.5",
        "1e-300",
        "9.99999999999999",
        "1234.5678",
        "0.30000000000000004",
        "1e100",
        "123.456",
        "1000000000000000.0",
        "round(2.0/3, 10)",
        "cast('3.14' as real)",
        "3.0",
        "-1.0/3",
    ];
    for e in exprs {
        let q = format!("SELECT {e}");
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(format!("{q};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let c = Connection::open_memory().unwrap();
        let got = match &c.query(&q).unwrap().rows[0][0] {
            Value::Real(r) => graphitesql::exec::eval::format_real(*r),
            Value::Integer(i) => i.to_string(),
            Value::Null => String::new(),
            other => format!("{other:?}"),
        };
        assert_eq!(got, want, "real formatting diverged: {e}");
    }
}
