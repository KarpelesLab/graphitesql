//! SQLite's `ieee754` extension functions — decompose/recompose a double into an
//! exact `mantissa · 2^exponent`, convert doubles to/from their 8-byte big-endian
//! blob, and step by ULPs. Ported from `ext/misc/ieee754.c`; byte-verified vs
//! sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn one(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}
fn text(c: &Connection, sql: &str) -> String {
    match one(c, sql) {
        Value::Text(s) => s.as_str().to_string(),
        v => panic!("not text: {v:?}"),
    }
}

#[test]
fn decompose_and_recompose() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT ieee754(45.25)"), "ieee754(181,-2)");
    assert_eq!(text(&c, "SELECT ieee754(2.0)"), "ieee754(2,0)");
    assert_eq!(text(&c, "SELECT ieee754(0.0)"), "ieee754(0,-1075)");
    // -0.0 keeps its sign bit → the (1,-3071) form sqlite produces.
    assert_eq!(text(&c, "SELECT ieee754(-0.0)"), "ieee754(1,-3071)");
    assert_eq!(one(&c, "SELECT ieee754(181,-2)"), Value::Real(45.25));
    assert_eq!(
        one(&c, "SELECT ieee754_mantissa(45.25)"),
        Value::Integer(181)
    );
    assert_eq!(
        one(&c, "SELECT ieee754_exponent(45.25)"),
        Value::Integer(-2)
    );
}

#[test]
fn blob_conversions_and_inc() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        one(&c, "SELECT ieee754_to_blob(1.0)"),
        Value::Blob(vec![0x3f, 0xf0, 0, 0, 0, 0, 0, 0])
    );
    assert_eq!(
        one(&c, "SELECT ieee754_from_blob(x'3ff0000000000000')"),
        Value::Real(1.0)
    );
    // Non-8-byte blob / NULL → NULL.
    assert_eq!(one(&c, "SELECT ieee754_from_blob(x'0102')"), Value::Null);
    // inc steps by ULPs: 0.0 + 1 ULP = the smallest subnormal.
    assert_eq!(
        one(&c, "SELECT ieee754_inc(0.0,1)"),
        Value::Real(f64::from_bits(1))
    );
    assert_eq!(
        one(&c, "SELECT ieee754_inc(1.0,1)"),
        Value::Real(f64::from_bits(1.0f64.to_bits() + 1))
    );
}

#[test]
fn coercion_and_null() {
    let c = Connection::open_memory().unwrap();
    // A numeric-looking text coerces; NULL coerces to 0.0 (not propagated), like
    // sqlite3_value_double.
    assert_eq!(text(&c, "SELECT ieee754('45.25')"), "ieee754(181,-2)");
    assert_eq!(text(&c, "SELECT ieee754(NULL)"), "ieee754(0,-1075)");
    assert_eq!(one(&c, "SELECT ieee754_mantissa(NULL)"), Value::Integer(0));
}
