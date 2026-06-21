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

/// Render a scalar the way the engine prints a result row, so the
/// 15-significant-digit text can be compared against `sqlite3`. `CAST(x AS TEXT)`
/// uses the same REAL→text formatting as a result column (verified to agree with
/// the CLI for `Inf`, scientific notation, and 15-sig values).
fn text(c: &Connection, sql: &str) -> String {
    match c
        .query(&format!("SELECT CAST(({sql}) AS TEXT)"))
        .unwrap()
        .rows[0][0]
        .clone()
    {
        Value::Null => String::new(),
        Value::Text(t) => t,
        other => format!("{other:?}"),
    }
}

/// `exp`/`pow`/`sinh`/`cosh` overflow to `Inf`, not NULL, exactly as SQLite. The
/// hard-coded strings are the observed `sqlite3` 3.50.4 outputs.
#[test]
fn overflow_is_infinity() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(one(&c, "SELECT exp(710)"), Value::Real(f64::INFINITY));
    assert_eq!(one(&c, "SELECT exp(1000)"), Value::Real(f64::INFINITY));
    assert_eq!(one(&c, "SELECT pow(2, 2000)"), Value::Real(f64::INFINITY));
    assert_eq!(one(&c, "SELECT pow(-2, 2000)"), Value::Real(f64::INFINITY));
    assert_eq!(
        one(&c, "SELECT pow(-2, 1025)"),
        Value::Real(f64::NEG_INFINITY)
    );
    assert_eq!(one(&c, "SELECT pow(0, -1)"), Value::Real(f64::INFINITY));
    assert_eq!(one(&c, "SELECT pow(0, -0.5)"), Value::Real(f64::INFINITY));
    assert_eq!(one(&c, "SELECT sinh(800)"), Value::Real(f64::INFINITY));
    assert_eq!(one(&c, "SELECT cosh(800)"), Value::Real(f64::INFINITY));
    assert_eq!(one(&c, "SELECT sinh(-800)"), Value::Real(f64::NEG_INFINITY));
    // Extreme arguments must not overflow the internal exponent cast.
    assert_eq!(one(&c, "SELECT exp(1e308)"), Value::Real(f64::INFINITY));
    assert_eq!(one(&c, "SELECT sinh(1e308)"), Value::Real(f64::INFINITY));
    assert_eq!(one(&c, "SELECT atanh(1)"), Value::Real(f64::INFINITY));
    assert_eq!(one(&c, "SELECT atanh(-1)"), Value::Real(f64::NEG_INFINITY));
    // Underflow: smallest subnormal, then flush to zero (matches SQLite).
    assert_eq!(one(&c, "SELECT exp(-745)"), Value::Real(f64::from_bits(1)));
    assert_eq!(one(&c, "SELECT exp(-746)"), Value::Real(0.0));
    assert_eq!(one(&c, "SELECT exp(-1000)"), Value::Real(0.0));
}

/// Domain errors return NULL (the result is NaN), distinct from overflow above.
#[test]
fn domain_errors_are_null() {
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT sqrt(-1)",
        "SELECT ln(0)",
        "SELECT ln(-1)",
        "SELECT log10(0)",
        "SELECT log2(0)",
        "SELECT log(0)",
        "SELECT log(0, 5)", // base 0
        "SELECT log(1, 5)", // base 1
        "SELECT log(5, 0)", // x = 0
        "SELECT log(5, -1)",
        "SELECT acos(2)",
        "SELECT asin(-2)",
        "SELECT acosh(0.5)",
        "SELECT atanh(2)",
        "SELECT pow(-8, 1.0/3)", // negative base, fractional exponent
    ] {
        assert_eq!(one(&c, sql), Value::Null, "{sql} should be NULL");
    }
}

/// Accuracy: the 15-significant-digit rendering now matches SQLite for cases the
/// old ~1-ulp-low `exp`/`pow` rendered differently. Strings are observed
/// `sqlite3` 3.50.4 output.
#[test]
fn rendering_matches_sqlite() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT exp(1)"), "2.71828182845905");
    assert_eq!(text(&c, "SELECT exp(709)"), "8.21840746155497e+307");
    assert_eq!(text(&c, "SELECT pow(2, 0.5)"), "1.4142135623731");
    assert_eq!(text(&c, "SELECT pow(0.5, -0.5)"), "1.4142135623731");
    assert_eq!(text(&c, "SELECT exp(710)"), "Inf");
    assert_eq!(text(&c, "SELECT sinh(-800)"), "-Inf");
}

/// IEEE signed-zero handling of `atan2`, matching SQLite's observed output.
#[test]
fn atan2_signed_zero_matches_sqlite() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT atan2(-0.0, -1)"), "-3.14159265358979");
    assert_eq!(text(&c, "SELECT atan2(0.0, -1)"), "3.14159265358979");
    assert_eq!(text(&c, "SELECT atan2(0.0, -0.0)"), "3.14159265358979");
    assert_eq!(text(&c, "SELECT atan2(-0.0, -0.0)"), "-3.14159265358979");
}

#[test]
fn modulo_operator_truncates_to_integer() {
    // The `%` operator truncates both operands to integers (unlike the mod()
    // function, which is a floating modulo), with a real result when an operand
    // is real, and NULL when the divisor truncates to zero — like SQLite.
    let c = Connection::open_memory().unwrap();
    let cell = |sql: &str| c.query(sql).unwrap().rows[0][0].clone();

    assert_eq!(cell("SELECT 10 % 3"), Value::Integer(1));
    assert_eq!(cell("SELECT 10.5 % 3"), Value::Real(1.0));
    assert_eq!(cell("SELECT 10.9 % 3.9"), Value::Real(1.0));
    assert_eq!(cell("SELECT -10.5 % 3"), Value::Real(-1.0));
    assert_eq!(cell("SELECT 7 % 2.5"), Value::Real(1.0));
    assert_eq!(cell("SELECT 5 % 1.5"), Value::Real(0.0));
    assert_eq!(cell("SELECT 5 % 0"), Value::Null);
    assert_eq!(cell("SELECT 5 % 0.5"), Value::Null); // divisor truncates to 0

    // The mod() function remains a floating modulo.
    assert_eq!(cell("SELECT mod(10.5, 3)"), Value::Real(1.5));
}
