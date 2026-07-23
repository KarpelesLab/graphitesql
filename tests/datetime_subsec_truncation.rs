//! Sub-millisecond fractional seconds are TRUNCATED to three digits, not
//! rounded, matching sqlite3 3.50.4's `parseHhMmSs` (`if( ms>0.999 ) ms = 0.999`).
//!
//! graphite parsed the fractional field with `sec += frac/scale` and no clamp, so
//! a value like `.9999` rounded up on the later `(s*1000 + 0.5)` step and spilled
//! into the next second — producing an impossible `:60` field (`13:45:59.9999`
//! rendered `13:45:60.000`). sqlite clamps the fractional part at parse time.
//!
//! Every expected value below is byte-verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;

fn t(c: &Connection, sql: &str) -> String {
    match c.query(sql).unwrap().rows.remove(0).remove(0) {
        graphitesql::Value::Text(s) => String::from(s.as_str()),
        other => panic!("expected text from {sql}, got {other:?}"),
    }
}

#[test]
fn strftime_f_truncates_sub_millisecond() {
    let c = Connection::open_memory().unwrap();
    // The reported case: `.9999` truncates to `.999`, never rounds to `31.000`.
    assert_eq!(
        t(&c, "SELECT strftime('%f','2024-01-15 13:45:30.9999')"),
        "30.999"
    );
    // The `.4994`/`.4995` boundary rounds within the millisecond as sqlite does.
    assert_eq!(
        t(&c, "SELECT strftime('%f','2024-01-15 13:45:30.4994')"),
        "30.499"
    );
    assert_eq!(
        t(&c, "SELECT strftime('%f','2024-01-15 13:45:30.4995')"),
        "30.500"
    );
    // ...but at the top of the second, `.999x` all stick at `.999` (no roll-over).
    for v in [".9994", ".9995", ".9996", ".9999"] {
        assert_eq!(
            t(
                &c,
                &format!("SELECT strftime('%f','2024-01-15 13:45:30{v}')")
            ),
            "30.999",
            "input {v}"
        );
    }
    // Assorted precisions.
    assert_eq!(
        t(&c, "SELECT strftime('%f','2024-01-15 13:45:30.123456')"),
        "30.123"
    );
    assert_eq!(
        t(&c, "SELECT strftime('%f','2024-01-15 13:45:30.0')"),
        "30.000"
    );
    assert_eq!(
        t(&c, "SELECT strftime('%f','2024-01-15 13:45:30.001')"),
        "30.001"
    );
}

#[test]
fn no_impossible_sixty_seconds_field() {
    let c = Connection::open_memory().unwrap();
    // The rounding bug produced `:60`; truncation keeps the field at `59.999`.
    assert_eq!(
        t(&c, "SELECT strftime('%H:%M:%f','2024-01-15 13:45:59.9999')"),
        "13:45:59.999"
    );
    assert_eq!(
        t(&c, "SELECT datetime('2024-01-15 23:59:59.9999','subsec')"),
        "2024-01-15 23:59:59.999"
    );
    assert_eq!(
        t(&c, "SELECT time('2024-01-15 13:45:59.9999','subsec')"),
        "13:45:59.999"
    );
}

#[test]
fn non_fractional_and_julian_paths_unaffected() {
    let c = Connection::open_memory().unwrap();
    // Whole-second and half-second inputs are unchanged.
    assert_eq!(
        t(&c, "SELECT strftime('%f','2024-01-15 13:45:30')"),
        "30.000"
    );
    assert_eq!(
        t(&c, "SELECT strftime('%f','2024-01-15 13:45:30.5')"),
        "30.500"
    );
    // julianday / %J follow the same clamp as sqlite (parse-time), so they match.
    assert_eq!(
        t(&c, "SELECT strftime('%J','2024-01-15 13:45:30.9999')"),
        "2460325.073275452"
    );
    // A plain time() (no subsec) still drops the fraction entirely.
    assert_eq!(t(&c, "SELECT time('2024-01-15 13:45:59.9999')"), "13:45:59");
}
