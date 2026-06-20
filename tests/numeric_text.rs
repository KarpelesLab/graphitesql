//! Numeric text rendering and parsing, matched to the `sqlite3` CLI (3.50.4).
//!
//! Three behaviors verified byte-for-byte against sqlite:
//!  1. `format_real` — the canonical real→text rendering (15 significant digits,
//!     `%g`-style fixed/exponential switch, `Inf`/`0.0`). The shell now uses this.
//!  2. CAST text→REAL takes the longest numeric prefix *including an exponent*
//!     (`'3.5e2xyz'` → 350.0), not just the mantissa.
//!  3. CAST text→INTEGER takes the leading *integer* prefix (`'2e2'` → 2, not
//!     200; `'2.9'` → 2), with overflow saturating to the i64 bounds.

#![cfg(feature = "std")]

use graphitesql::exec::eval::format_real;
use graphitesql::{Connection, Value};

/// Oracle values are the exact strings printed by `sqlite3` 3.50.4.
#[test]
fn format_real_matches_sqlite() {
    // Whole-number reals keep a `.0`; ordinary reals print up to 15 sig digits.
    assert_eq!(format_real(1000000.0), "1000000.0");
    assert_eq!(format_real(123.456), "123.456");
    assert_eq!(format_real(2.0 / 3.0), "0.666666666666667");
    assert_eq!(format_real(0.1), "0.1");
    assert_eq!(format_real(10000000000.5), "10000000000.5");
    // Switch to exponential when the decimal exponent is >= 15 or < -4.
    assert_eq!(format_real(1e15), "1.0e+15");
    assert_eq!(format_real(1e16), "1.0e+16");
    assert_eq!(format_real(1e18), "1.0e+18");
    assert_eq!(format_real(1e20), "1.0e+20");
    assert_eq!(format_real(1234567890123456.0), "1.23456789012346e+15");
    assert_eq!(format_real(1.5e300), "1.5e+300");
    assert_eq!(format_real(1e308), "1.0e+308");
    assert_eq!(format_real(1e-5), "1.0e-05");
    // ...but 1e-4 stays fixed.
    assert_eq!(format_real(1e-4), "0.0001");
    assert_eq!(format_real(0.0001), "0.0001");
    // Zero (either sign) and infinities.
    assert_eq!(format_real(0.0), "0.0");
    assert_eq!(format_real(-0.0), "0.0");
    assert_eq!(format_real(f64::INFINITY), "Inf");
    assert_eq!(format_real(f64::NEG_INFINITY), "-Inf");
    // Integer overflow of `+` promotes to REAL, rendered in exponential form.
    assert_eq!(
        format_real(9223372036854775807.0 + 1.0),
        "9.22337203685478e+18"
    );
}

fn real(c: &Connection, sql: &str) -> f64 {
    match c.query(sql).unwrap().rows.remove(0).remove(0) {
        Value::Real(r) => r,
        other => panic!("expected real from {sql}, got {other:?}"),
    }
}

fn int(c: &Connection, sql: &str) -> i64 {
    match c.query(sql).unwrap().rows.remove(0).remove(0) {
        Value::Integer(i) => i,
        other => panic!("expected integer from {sql}, got {other:?}"),
    }
}

#[test]
fn cast_text_to_real_consumes_exponent() {
    let c = Connection::open_memory().unwrap();
    // The numeric prefix includes a valid exponent.
    assert_eq!(real(&c, "SELECT CAST('3.5e2xyz' AS REAL)"), 350.0);
    assert_eq!(real(&c, "SELECT CAST('1e2' AS REAL)"), 100.0);
    assert_eq!(real(&c, "SELECT CAST('1e+2' AS REAL)"), 100.0);
    assert_eq!(real(&c, "SELECT CAST('.5e2' AS REAL)"), 50.0);
    // A bare `e` (no exponent digits) is trailing junk: the prefix stops before it.
    assert_eq!(real(&c, "SELECT CAST('1e' AS REAL)"), 1.0);
    assert_eq!(real(&c, "SELECT CAST('1e+' AS REAL)"), 1.0);
    assert_eq!(real(&c, "SELECT CAST('12abc' AS REAL)"), 12.0);
    assert_eq!(real(&c, "SELECT CAST('abc' AS REAL)"), 0.0);
    // The same longest-prefix rule drives implicit numeric coercion.
    assert_eq!(real(&c, "SELECT '3.5e2xyz'+0"), 350.0);
    assert_eq!(real(&c, "SELECT '1e3abc' * 2"), 2000.0);
}

#[test]
fn cast_text_to_integer_takes_integer_prefix() {
    let c = Connection::open_memory().unwrap();
    // Stops at `.` or `e` — never reads the float value.
    assert_eq!(int(&c, "SELECT CAST('2e2' AS INTEGER)"), 2);
    assert_eq!(int(&c, "SELECT CAST('2.9' AS INTEGER)"), 2);
    assert_eq!(int(&c, "SELECT CAST('-4.6' AS INTEGER)"), -4);
    assert_eq!(int(&c, "SELECT CAST('1e100' AS INTEGER)"), 1);
    assert_eq!(int(&c, "SELECT CAST('  -12abc' AS INTEGER)"), -12);
    assert_eq!(int(&c, "SELECT CAST('+7' AS INTEGER)"), 7);
    assert_eq!(int(&c, "SELECT CAST('12 34' AS INTEGER)"), 12);
    // No integer prefix → 0.
    assert_eq!(int(&c, "SELECT CAST('.5' AS INTEGER)"), 0);
    assert_eq!(int(&c, "SELECT CAST('0x10' AS INTEGER)"), 0);
    assert_eq!(int(&c, "SELECT CAST('   ' AS INTEGER)"), 0);
    assert_eq!(int(&c, "SELECT CAST('-' AS INTEGER)"), 0);
    // Overflow saturates to the i64 bounds (matching sqlite).
    assert_eq!(
        int(&c, "SELECT CAST('99999999999999999999' AS INTEGER)"),
        i64::MAX
    );
    assert_eq!(
        int(&c, "SELECT CAST('-99999999999999999999' AS INTEGER)"),
        i64::MIN
    );
    assert_eq!(
        int(&c, "SELECT CAST('9223372036854775808' AS INTEGER)"),
        i64::MAX
    );
    assert_eq!(
        int(&c, "SELECT CAST('-9223372036854775808' AS INTEGER)"),
        i64::MIN
    );
    // Blobs reinterpret their bytes as text first: X'3132' = "12".
    assert_eq!(int(&c, "SELECT CAST(X'3132' AS INTEGER)"), 12);
}
