//! `printf`/`format` float conversions (`%g`/`%e`/`%f`) cap at 16 significant
//! digits and pad any further requested digits with zeros — matching the sqlite3
//! 3.50.4 CLI, whose float renderer is not C's. C would expose the true trailing
//! digits (`%.20f` of 0.1 → `0.10000000000000000555`); sqlite (and now graphite)
//! emit `0.10000000000000000000`. The `!` (alt-form-2) flag lifts the cap to
//! SQLite's `mxRound = 26` — but still through SQLite's own `sqlite3FpDecode`
//! decimal machinery (ported in `util::fpdecode`), NOT C's exact-f64 expansion —
//! so `%!.20f` of 0.1 is `0.1000000000000000055`, matching the CLI (see
//! `printf_bang_fpdecode.rs`).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn text(c: &Connection, sql: &str) -> String {
    match c.query(sql).unwrap().rows[0][0].clone() {
        Value::Text(s) => String::from(s.as_str()),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn high_precision_pads_with_zeros_not_true_digits() {
    let c = Connection::open_memory().unwrap();
    // %g beyond the shortest representation: zeros, then stripped by %g.
    assert_eq!(text(&c, "SELECT printf('%.17g', 0.1)"), "0.1");
    assert_eq!(text(&c, "SELECT printf('%.25g', 0.3)"), "0.3");
    // 0.1+0.2 (=0.30000000000000004) collapses under the 16-digit cap.
    assert_eq!(text(&c, "SELECT printf('%.17g', 0.1+0.2)"), "0.3");
    // %f / %e pad the tail with zeros past the 16 significant digits.
    assert_eq!(
        text(&c, "SELECT printf('%.20f', 0.1)"),
        "0.10000000000000000000"
    );
    assert_eq!(
        text(&c, "SELECT printf('%.20e', 0.1)"),
        "1.00000000000000000000e-01"
    );
}

#[test]
fn sixteen_significant_digits_are_kept() {
    let c = Connection::open_memory().unwrap();
    // A value needing all 16 digits keeps them (printf uses one more digit than
    // value-to-text rendering, which caps at 15).
    assert_eq!(
        text(&c, "SELECT printf('%.20g', 1.0/3.0)"),
        "0.3333333333333333"
    );
    assert_eq!(
        text(&c, "SELECT printf('%.20e', 1.0/3.0)"),
        "3.33333333333333300000e-01"
    );
    // A 17-digit literal is rounded to 16 significant digits.
    assert_eq!(
        text(&c, "SELECT printf('%.20g', 1.2345678901234567)"),
        "1.234567890123457"
    );
}

#[test]
fn bang_flag_uses_full_precision() {
    let c = Connection::open_memory().unwrap();
    // The `!` flag lifts the 16-digit cap, exposing the true stored value.
    assert_eq!(
        text(&c, "SELECT printf('%!.17g', 0.1)"),
        "0.10000000000000001"
    );
    // Ordinary (non-bang) high precision still caps.
    assert_eq!(text(&c, "SELECT printf('%.17g', 0.1)"), "0.1");
}

#[test]
fn normal_precision_is_unchanged() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT printf('%g', 0.1)"), "0.1");
    assert_eq!(text(&c, "SELECT printf('%e', 1.0)"), "1.000000e+00");
    assert_eq!(text(&c, "SELECT printf('%f', 1.5)"), "1.500000");
    assert_eq!(text(&c, "SELECT printf('%.6g', 123.456)"), "123.456");
    assert_eq!(text(&c, "SELECT printf('%.2g', 0.666)"), "0.67");
}
