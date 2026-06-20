//! Differential testing of round() — half-away-from-zero rounding on the true
//! decimal value (not the lossy x*10^n product), checked against sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

#[test]
fn round_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = Connection::open_memory().unwrap();
    let exprs = [
        "round(2.5)",
        "round(3.5)",
        "round(0.5)",
        "round(-0.5)",
        "round(-1.5)",
        "round(2.675,2)",
        "round(1.005,2)",
        "round(2.5,0)",
        "round(123.456,-1)",
        "round(9.99)",
        "round(9.99,1)",
        "round(-2.675,2)",
        "round(0.125,2)",
        "round(0.135,2)",
        "round(2.345,2)",
        "round(100.5)",
        "round(1234.5678,3)",
        "round(0.49999999,0)",
        "round(2.0/3,5)",
        "round(99.95,1)",
        "round(0.0)",
        "round(-0.0)",
        "round(1000000.5)",
        "round(0.00001,2)",
        "round(12345.6789,2)",
        "round(-99.995,2)",
        "round(0.5,0)",
        "round(2.55,1)",
        "round(8.005,2)",
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
        let got = match &c.query(&q).unwrap().rows[0][0] {
            Value::Real(r) => graphitesql::exec::eval::format_real(*r),
            Value::Integer(i) => i.to_string(),
            Value::Null => String::new(),
            other => format!("{other:?}"),
        };
        assert_eq!(got, want, "round diverged: {e}");
    }
}
