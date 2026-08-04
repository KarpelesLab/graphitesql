//! sin/cos/tan are correct (and bounded in [-1,1] for sin/cos) for ALL finite
//! arguments, including huge ones, matching the system libm that sqlite3 uses.
//! graphite's old Cody–Waite reduction overflowed i64 for large |x| and returned
//! garbage (e.g. `sin(1e20)` = -1.5e45); a Payne–Hanek reduction fixes it.
//! Byte/near-exact vs sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::Connection;

fn f(c: &Connection, sql: &str) -> f64 {
    match &c.query(sql).unwrap().rows[0][0] {
        graphitesql::Value::Real(r) => *r,
        graphitesql::Value::Integer(n) => *n as f64,
        v => panic!("not a number: {v:?}"),
    }
}

#[test]
fn sin_cos_are_bounded_for_huge_arguments() {
    let c = Connection::open_memory().unwrap();
    for x in ["1e15", "1e18", "1e20", "1e100", "1e300", "-1e20", "-1e250"] {
        let s = f(&c, &format!("SELECT sin({x})"));
        let co = f(&c, &format!("SELECT cos({x})"));
        assert!(s.abs() <= 1.0 + 1e-12, "sin({x}) = {s} out of range");
        assert!(co.abs() <= 1.0 + 1e-12, "cos({x}) = {co} out of range");
        // Pythagorean identity as an internal consistency check.
        assert!(
            (s * s + co * co - 1.0).abs() < 1e-9,
            "sin²+cos² off for {x}"
        );
    }
}

#[test]
fn large_argument_values_match_reference() {
    let c = Connection::open_memory().unwrap();
    // Reference values from sqlite3 3.50.4 (system libm).
    let cases = [
        ("sin(1e20)", -0.645251285265781_f64),
        ("cos(1e20)", 0.763970404441728),
        ("sin(1e18)", -0.992969320740405),
        ("sin(1e100)", -0.380637731005029),
        ("cos(1e300)", -0.575386111957549),
        ("tan(1e20)", -0.844602463019884),
    ];
    for (expr, want) in cases {
        let got = f(&c, &format!("SELECT {expr}"));
        assert!(
            (got - want).abs() / (want.abs() + 1e-300) < 1e-9,
            "{expr}: got {got}, want {want}"
        );
    }
}
