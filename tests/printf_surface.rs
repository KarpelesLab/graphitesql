//! Full-surface differential coverage for `printf`/`format`, matched against the
//! `sqlite3` CLI (the differential oracle). Covers every conversion specifier,
//! flag (`- + space 0 # , !`), `*` width/precision, integer minimum-digit
//! precision, the SQLite extensions (`%q %Q %w %z`), argument-type coercion,
//! NULL/too-few arguments, non-finite floats, and the field-width ceiling.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Render graphite's value for an expression. Results are wrapped in `[...]` by
/// the caller so that significant leading/trailing whitespace survives the
/// `sqlite3` CLI's line trimming.
fn graphite(expr: &str) -> String {
    let c = Connection::open_memory().unwrap();
    match &c.query(&format!("SELECT {expr}")).unwrap().rows[0][0] {
        graphitesql::Value::Text(t) => t.clone(),
        graphitesql::Value::Integer(i) => i.to_string(),
        graphitesql::Value::Real(r) => r.to_string(),
        graphitesql::Value::Null => String::from("<null>"),
        other => format!("{other:?}"),
    }
}

fn oracle(expr: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("SELECT '['||({expr})||']';"))
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    let s = s.trim_end_matches('\n');
    // Strip the bracket guards we added to preserve whitespace.
    s.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(s)
        .to_string()
}

/// Assert graphite byte-matches the live `sqlite3` CLI for every expression.
fn diff_all(exprs: &[&str]) {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    for e in exprs {
        assert_eq!(graphite(e), oracle(e), "diverged on {e}");
    }
}

#[test]
fn alt_form_hash_flag() {
    diff_all(&[
        "printf('%#x', 255)",   // 0xff
        "printf('%#X', 255)",   // 0XFF
        "printf('%#o', 8)",     // 010
        "printf('%#o', 0)",     // 0 (no prefix on zero)
        "printf('%#x', 0)",     // 0 (no prefix on zero)
        "printf('%#5x', 255)",  //  0xff
        "printf('%#8x', 255)",  //     0xff
        "printf('%#-8x', 255)", // 0xff
        "printf('%#08x', 255)", // 0x000000ff (prefix not counted by width)
        "printf('%#010x', 255)",
        "printf('%#06o', 255)",
        "printf('%#.5x', 255)",
        "printf('%#.5o', 8)",
        "printf('%#d', 5)", // no effect on decimal
        "printf('%#g', 1.0)",
        "printf('%#.3g', 1.5)",
        "printf('%#f', 1.0)",
        "printf('%#.0f', 1.0)", // 1.
        "printf('%#e', 1.0)",
    ]);
}

#[test]
fn integer_precision() {
    diff_all(&[
        "printf('%.3d', 7)",   // 007
        "printf('%5.3d', 7)",  //   007
        "printf('%-5.3d', 7)", // 007
        "printf('%05.3d', 7)", // 00007 (width still zero-pads)
        "printf('%08.3d', 7)",
        "printf('%-08.3d', 7)",
        "printf('%.0d', 0)", // 0 (SQLite, unlike C, keeps it)
        "printf('%5.0d', 0)",
        "printf('%.0x', 0)",
        "printf('%.0o', 0)",
        "printf('%.3d', -7)",
        "printf('%8.3d', -7)",
        "printf('%.5x', 255)",
        "printf('%.5o', 8)",
        "printf('%+8.3d', 7)",
        "printf('%-8.3d', 7)",
    ]);
}

#[test]
fn comma_thousands_flag() {
    diff_all(&[
        "printf('%,d', 1000000)",
        "printf('%,d', -1000000)",
        "printf('%,d', 100)",
        "printf('%,d', 1000)",
        "printf('%,f', 1234567.5)",
        "printf('%,12d', 1000)",  // comma before width: works
        "printf('%,012d', 1000)", // zero-padded grouped output
        "printf('%+,d', 1000)",
        "printf('% ,d', 1000)",
        "printf('%-,d', 1000)",
        "printf('%,x', 255)",      // grouping ignored for hex
        "printf('%,s', 'hi')",     // grouping ignored for strings
        "printf('%12,d', 1000)",   // width before comma: empty (invalid)
        "printf('%10,f', 1234.5)", // width before comma: empty
        "printf('%,d', 1234567890123)",
    ]);
}

#[test]
fn bang_alt_round_flag() {
    diff_all(&[
        "printf('%!g', 1.0)",
        "printf('%!g', 0.1)",
        "printf('%!g', 100000.0)",
        "printf('%!g', 1000000.0)",
        "printf('%!.17g', 0.1)",
        "printf('%!f', 0.1)",
        "printf('%!f', 1.0)",
        "printf('%!f', 100.0)",
        "printf('%!.10f', 0.1)",
        "printf('%!.0f', 1.5)",
        "printf('%!.6f', 1.2345)",
        "printf('%!e', 0.1)",
        "printf('%!e', 1.0)",
        "printf('%!e', 100.0)",
        "printf('%!.3e', 1.5)",
        "printf('%!G', 1e-06)",
        "printf('%!d', 5)", // no effect on integers
    ]);
}

#[test]
fn conversions_basic() {
    diff_all(&[
        "printf('%d', 42)",
        "printf('%i', 42)",
        "printf('%u', 42)",
        "printf('%u', -1)",
        "printf('%x', 255)",
        "printf('%X', 255)",
        "printf('%o', 8)",
        "printf('%x', -1)",
        "printf('%X', -255)",
        "printf('%o', -1)",
        "printf('%f', 3.5)",
        "printf('%e', 12345.678)",
        "printf('%E', 12345.678)",
        "printf('%g', 100000.0)",
        "printf('%g', 1000000.0)",
        "printf('%G', 1e-06)",
        "printf('%s', 'hi')",
        "printf('%c', 'hello')",
        "printf('%c', 104)",
        "printf('%c', 65)",
        "printf('100%%')",
    ]);
}

#[test]
fn sqlite_extensions_q_big_q_w_z() {
    diff_all(&[
        "printf('%q', 'a''b')",
        "printf('%q', 'plain')",
        "printf('%q', NULL)", // (NULL)
        "printf('%Q', 'a''b')",
        "printf('%Q', 'hi')",
        "printf('%Q', 5)",
        "printf('%Q', NULL)", // NULL
        "printf('%w', 'a\"b')",
        "printf('%w', NULL)", // (NULL)
        "printf('%z', 'hello')",
        "printf('%.2z', 'hello')",
        "printf('%.1q', 'abc')",
        "printf('%10q', 'ab')",
        "printf('%10Q', 'ab')",
        "printf('%-10w', 'ab')",
    ]);
}

#[test]
fn string_width_precision() {
    diff_all(&[
        "printf('%.3s', 'hello')",
        "printf('%5.3s', 'hello')",
        "printf('%-5.3s', 'hello')",
        "printf('%5s', 'ab')",
        "printf('%-5s|', 'ab')",
        "printf('%s', 5)",
        "printf('%s', 3.5)",
    ]);
}

#[test]
fn star_width_and_precision() {
    diff_all(&[
        "printf('%*d', 5, 7)",
        "printf('%-*d', 5, 7)",
        "printf('%*d', -5, 7)", // negative width => left-justify
        "printf('%.*f', 2, 3.14159)",
        "printf('%*.*f', 8, 2, 3.14159)",
        "printf('%.*s', 3, 'hello')",
    ]);
}

#[test]
fn sign_and_space_flags() {
    diff_all(&[
        "printf('%+d', 5)",
        "printf('%+d', -5)",
        "printf('% d', 5)",
        "printf('% d', -5)",
        "printf('%+08d', 7)",
        "printf('% 08d', 7)",
        "printf('%+f', 1.5)",
        "printf('% f', 1.5)",
        "printf('%+.2f', -1.5)",
    ]);
}

#[test]
fn zero_and_left_flag_interaction() {
    diff_all(&[
        "printf('%-8d', 7)",
        "printf('%-08d', 7)", // SQLite zero-pads here (0 not overridden by -)
        "printf('%08d', 7)",
        "printf('%08f', 1.5)",
        "printf('%-8f|', 1.5)",
    ]);
}

#[test]
fn coercion_int_real() {
    diff_all(&[
        "printf('%d', 3.9)",
        "printf('%d', 3.2)",
        "printf('%d', -3.9)",
        "printf('%f', 5)",
        "printf('%x', 255.9)",
        "printf('%o', 8.9)",
        "printf('%d', -0.0)",
        "printf('%f', -0.0)",
        "printf('%g', -0.0)",
    ]);
}

#[test]
fn null_and_missing_arguments() {
    diff_all(&[
        "printf('%d', NULL)",
        "printf('%f', NULL)",
        "printf('%s', NULL)",
        "printf('%x', NULL)",
        "printf('%d %d', 5)", // too few => 0 for the rest
        "printf('%d %s', 5)",
        "printf('%g', NULL)",
    ]);
    // `%c` of NULL emits a single NUL byte in SQLite (hex `00`); the `sqlite3`
    // CLI can't carry a NUL through stdout, so assert it directly.
    assert_eq!(graphite("printf('%c', NULL)"), "\0");
}

#[test]
fn non_finite_floats() {
    diff_all(&[
        "printf('%f', 1e400)",
        "printf('%f', -1e400)",
        "printf('%g', 1e400)",
        "printf('%e', 1e400)",
        "printf('%5f', 1e400)",
        "printf('%-8f|', 1e400)",
        "printf('%+f', 1e400)",
        "printf('%!f', 1e400)",
    ]);
}

#[test]
fn g_notation_forms() {
    diff_all(&[
        "printf('%g', 0.0001)",
        "printf('%g', 0.00001)",
        "printf('%g', 0.0)",
        "printf('%g', 1.5)",
        "printf('%.3g', 3.14159)",
        "printf('%.0g', 123.0)",
        "printf('%g', 10000000.0)",
    ]);
}

#[test]
fn unknown_conversions_truncate() {
    // An unknown conversion makes SQLite discard the directive and the rest of
    // the format string.
    diff_all(&[
        "printf('a%yb', 5)",
        "printf('%y', 5)",
        "printf('a%kb')",
        "printf('a-%v-b', 1)",
    ]);
}

#[test]
fn huge_width_returns_empty() {
    // Past SQLITE_MAX_LENGTH the field is rejected and the whole result is "".
    diff_all(&[
        "printf('%2000000000d', 1)",
        "printf('%1000000000d', 1)",
        "length(printf('%999999999d', 1))",
        "length(printf('%1000d', 1))",
        "length(printf('%.100000f', 1.5))",
    ]);
}

#[test]
fn i64_min_does_not_panic() {
    diff_all(&[
        "printf('%d', -9223372036854775808)",
        "printf('%x', -9223372036854775808)",
        "printf('%,d', -9223372036854775808)",
        "printf('%u', -9223372036854775808)",
    ]);
}

#[test]
fn huge_precision_does_not_panic() {
    diff_all(&[
        "length(printf('%.100000f', 1.5))",
        "length(printf('%.100000e', 1.5))",
        "length(printf('%.100000g', 1.5))",
        "length(printf('%.70000e', 1.5))",
    ]);
}

#[test]
fn width_precision_float_and_misc() {
    diff_all(&[
        "printf('%8.3f', 3.14159)",
        "printf('%-8.3f|', 3.14159)",
        "printf('%08.3f', 3.14159)",
        "printf('%10.4e', 12345.678)",
        "printf('%-10.4e|', 12345.678)",
        "printf('%12.6g', 3.14159)",
        "printf('%c', 1000)", // first char of "1000"
        "printf('%5c', 'x')",
        "printf('%-5c|', 'x')",
        "printf('%+.0f', 0.4)",
        "printf('%.0f', 0.5)",
        "printf('%.0f', 2.5)",
        "printf('%#.3g', 100.0)",
        "printf('hello')", // no conversions
        "printf('%d%%done', 50)",
    ]);
}
