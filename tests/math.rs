//! Track A: SQLite math functions (`sqrt`, `exp`, `ln`, `pow`, trig, …).
//! Implemented in pure `core` arithmetic (no libm dependency); validated for
//! correctness against the `sqlite3` CLI within a tight floating tolerance.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn one(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Real(r) => Some(*r),
        Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

#[test]
fn scalar_values() {
    let c = Connection::open_memory().unwrap();
    let approx = |sql: &str, want: f64| {
        let got = as_f64(&one(&c, sql)).expect("numeric");
        assert!(
            (got - want).abs() <= 1e-12 * (1.0 + want.abs()),
            "{sql}: got {got}, want {want}"
        );
    };
    approx("SELECT pi()", std::f64::consts::PI);
    approx("SELECT sqrt(2)", std::f64::consts::SQRT_2);
    approx("SELECT sqrt(16)", 4.0);
    approx("SELECT exp(1)", std::f64::consts::E);
    approx("SELECT ln(10)", 10f64.ln());
    approx("SELECT log(100)", 2.0); // base-10
    approx("SELECT log(2, 8)", 3.0); // base-2
    approx("SELECT log10(1000)", 3.0);
    approx("SELECT log2(1024)", 10.0);
    approx("SELECT pow(2, 10)", 1024.0);
    approx("SELECT power(2, 0.5)", std::f64::consts::SQRT_2);
    approx("SELECT sin(1)", 1f64.sin());
    approx("SELECT cos(1)", 1f64.cos());
    approx("SELECT tan(1)", 1f64.tan());
    approx("SELECT asin(0.5)", 0.5f64.asin());
    approx("SELECT acos(0.5)", 0.5f64.acos());
    approx("SELECT atan(2)", 2f64.atan());
    approx("SELECT atan2(1, 3)", 1f64.atan2(3.0));
    approx("SELECT sinh(1)", 1f64.sinh());
    approx("SELECT cosh(1)", 1f64.cosh());
    approx("SELECT tanh(0.5)", 0.5f64.tanh());
    approx("SELECT asinh(1)", 1f64.asinh());
    approx("SELECT acosh(2)", 2f64.acosh());
    approx("SELECT atanh(0.5)", 0.5f64.atanh());
    approx("SELECT ceil(2.1)", 3.0);
    approx("SELECT floor(-2.1)", -3.0);
    approx("SELECT trunc(2.7)", 2.0);
    approx("SELECT degrees(pi())", 180.0);
    approx("SELECT radians(180)", std::f64::consts::PI);
    approx("SELECT mod(7, 3)", 1.0);
}

#[test]
fn null_and_domain() {
    let c = Connection::open_memory().unwrap();
    // NULL argument → NULL.
    assert_eq!(one(&c, "SELECT sqrt(NULL)"), Value::Null);
    assert_eq!(one(&c, "SELECT sin(NULL)"), Value::Null);
    // Domain errors → NULL (sqrt of negative, ln of non-positive).
    assert_eq!(one(&c, "SELECT sqrt(-1)"), Value::Null);
    assert_eq!(one(&c, "SELECT ln(0)"), Value::Null);
    assert_eq!(one(&c, "SELECT ln(-2)"), Value::Null);
    assert_eq!(one(&c, "SELECT acos(2)"), Value::Null);
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let exprs = [
        "sqrt(2)",
        "sqrt(150.25)",
        "exp(2.5)",
        "ln(7)",
        "log(1000)",
        "log(2, 100)",
        "log2(40)",
        "pow(3, 4)",
        "pow(2.0, 0.3)",
        "sin(0.7)",
        "cos(2.3)",
        "tan(1.1)",
        "asin(0.3)",
        "acos(0.9)",
        "atan(5)",
        "atan2(2, 7)",
        "sinh(2)",
        "cosh(1.5)",
        "tanh(0.9)",
        "ceil(9.0001)",
        "floor(9.999)",
        "degrees(1)",
        "radians(57)",
        "mod(17.5, 4)",
    ];
    let select = format!("SELECT {}", exprs.join(", "));
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(&select)
        .output()
        .unwrap();
    let want_line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let want: Vec<f64> = want_line
        .split('|')
        .map(|s| s.parse::<f64>().unwrap())
        .collect();

    let c = Connection::open_memory().unwrap();
    let r = c.query(&select).unwrap();
    let mut failures = Vec::new();
    for (i, e) in exprs.iter().enumerate() {
        let got = as_f64(&r.rows[0][i]).unwrap();
        let w = want[i];
        if (got - w).abs() > 1e-11 * (1.0 + w.abs()) {
            failures.push(format!("  {e}: sqlite={w}, graphite={got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} math exprs diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
