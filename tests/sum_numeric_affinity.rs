//! `sum()` must decide its result type (INTEGER vs REAL) from each argument's
//! *numeric type* — `sqlite3_value_numeric_type` in SQLite's `sumStep` — not
//! from its storage class.
//!
//! SQLite keeps a `sum()` exact (INTEGER) only when every input's numeric type
//! is INTEGER: an INTEGER value, or a TEXT value that is a pure signed integer
//! (optional surrounding whitespace, no decimal point or exponent, fitting in
//! an `i64`). A REAL value — or any text/blob that is real-valued, overflows an
//! `i64`, or is non-numeric — forces a REAL result. graphite previously keyed
//! off the storage class alone, so a numeric-integer *text* like `'1'` wrongly
//! promoted the whole sum to REAL (`sum('1', 10)` → `11.0` instead of `11`).
//! The same rule governs the windowed form `sum(x) OVER (...)`.
//!
//! Verified against sqlite3 3.50.4, with the VDBE both on (default) and off.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

/// One scalar cell of the first result row, as the SQLite storage class +
/// rendered value (so we assert the *type* as well as the number).
fn cell(c: &Connection, sql: &str) -> Result<Value, String> {
    c.query(sql)
        .map(|r| {
            r.rows
                .into_iter()
                .next()
                .map(|mut row| row.remove(0))
                .unwrap()
        })
        .map_err(|e| {
            let s = e.to_string();
            s.strip_prefix("error: ").unwrap_or(&s).to_string()
        })
}

#[test]
fn sum_result_type_follows_numeric_affinity() {
    for &vdbe in &[true, false] {
        let c = Connection::open_memory().unwrap();
        c.set_use_vdbe(vdbe);

        // A pure-integer text keeps the sum exact (INTEGER).
        assert_eq!(
            cell(&c, "SELECT sum(a) FROM (SELECT '1' a UNION ALL SELECT 10)"),
            Ok(Value::Integer(11)),
            "vdbe={vdbe}"
        );
        // Surrounding whitespace and a leading sign are still a pure integer.
        assert_eq!(
            cell(
                &c,
                "SELECT sum(a) FROM (SELECT '  10  ' a UNION ALL SELECT 5)"
            ),
            Ok(Value::Integer(15)),
            "vdbe={vdbe}"
        );
        assert_eq!(
            cell(
                &c,
                "SELECT sum(a) FROM (SELECT '+5' a UNION ALL SELECT '-2')"
            ),
            Ok(Value::Integer(3)),
            "vdbe={vdbe}"
        );
        // Real-syntax text (decimal point / exponent) forces REAL.
        assert_eq!(
            cell(
                &c,
                "SELECT sum(a) FROM (SELECT '1.0' a UNION ALL SELECT 10)"
            ),
            Ok(Value::Real(11.0)),
            "vdbe={vdbe}"
        );
        assert_eq!(
            cell(
                &c,
                "SELECT sum(a) FROM (SELECT '1e2' a UNION ALL SELECT 10)"
            ),
            Ok(Value::Real(110.0)),
            "vdbe={vdbe}"
        );
        // Non-numeric text contributes 0 and forces REAL.
        assert_eq!(
            cell(
                &c,
                "SELECT sum(a) FROM (SELECT 'abc' a UNION ALL SELECT 10)"
            ),
            Ok(Value::Real(10.0)),
            "vdbe={vdbe}"
        );
        // An integer string too large for i64 is REAL (it is not integer-typed).
        assert!(
            matches!(
                cell(
                    &c,
                    "SELECT sum(a) FROM (SELECT '9999999999999999999' a UNION ALL SELECT 1)"
                ),
                Ok(Value::Real(_))
            ),
            "vdbe={vdbe}"
        );
        // A genuine REAL input forces REAL even when integral.
        assert_eq!(
            cell(&c, "SELECT sum(a) FROM (SELECT 1.0 a UNION ALL SELECT 2)"),
            Ok(Value::Real(3.0)),
            "vdbe={vdbe}"
        );
        // All-integer stays exact.
        assert_eq!(
            cell(&c, "SELECT sum(a) FROM (SELECT 1 a UNION ALL SELECT 2)"),
            Ok(Value::Integer(3)),
            "vdbe={vdbe}"
        );
        // Empty input is NULL.
        assert_eq!(
            cell(&c, "SELECT sum(a) FROM (SELECT 1 a WHERE 0)"),
            Ok(Value::Null),
            "vdbe={vdbe}"
        );
        // The windowed form obeys the same rule.
        assert_eq!(
            cell(
                &c,
                "SELECT typeof(sum(a) OVER ()) FROM (SELECT '1' a UNION ALL SELECT 10)"
            ),
            Ok(Value::Text("integer".into())),
            "vdbe={vdbe}"
        );
        // An all-integer sum that overflows i64 is an error (plain and windowed).
        assert_eq!(
            cell(
                &c,
                "SELECT sum(a) FROM (SELECT 9223372036854775807 a UNION ALL SELECT 1)"
            ),
            Err("integer overflow".into()),
            "vdbe={vdbe}"
        );
        assert_eq!(
            cell(
                &c,
                "SELECT sum(a) OVER () FROM (SELECT 9223372036854775807 a UNION ALL SELECT 1)"
            ),
            Err("integer overflow".into()),
            "vdbe={vdbe}"
        );
    }
}

#[test]
fn matches_sqlite_cli() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        let e = String::from_utf8_lossy(&out.stderr);
        let et = e.trim();
        if !et.is_empty() {
            return et
                .lines()
                .next()
                .unwrap_or("")
                .trim_start_matches("Error: in prepare, ")
                .trim_start_matches("Error: ")
                .trim_start_matches("error: ")
                .trim_start_matches("stepping, ")
                .to_string();
        }
        s.lines().collect::<Vec<_>>().join("|")
    };
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        "SELECT sum(a), typeof(sum(a)) FROM (SELECT '1' a UNION ALL SELECT 10);",
        "SELECT a, sum(a) OVER (ORDER BY a) FROM (SELECT '1' a UNION ALL SELECT 10);",
        "SELECT typeof(sum(a) OVER ()) FROM (SELECT '1' a UNION ALL SELECT 10);",
        "SELECT sum(a), typeof(sum(a)) FROM (SELECT '1.5' a UNION ALL SELECT 10);",
        "SELECT sum(a), typeof(sum(a)) FROM (SELECT '1.0' a UNION ALL SELECT 10);",
        "SELECT sum(a), typeof(sum(a)) FROM (SELECT '1e2' a UNION ALL SELECT 10);",
        "SELECT sum(a), typeof(sum(a)) FROM (SELECT 'abc' a UNION ALL SELECT 10);",
        "SELECT sum(a), typeof(sum(a)) FROM (SELECT '1abc' a UNION ALL SELECT 10);",
        "SELECT sum(a), typeof(sum(a)) FROM (SELECT '  10  ' a UNION ALL SELECT 5);",
        "SELECT sum(a), typeof(sum(a)) FROM (SELECT '+5' a UNION ALL SELECT 5);",
        "SELECT sum(a), typeof(sum(a)) FROM (SELECT '-5' a UNION ALL SELECT 5);",
        "SELECT sum(a), typeof(sum(a)) FROM (SELECT 1 a UNION ALL SELECT 2);",
        "SELECT sum(a), typeof(sum(a)) FROM (SELECT 1.0 a UNION ALL SELECT 2);",
        "SELECT sum(a) FROM (SELECT 1 a WHERE 0);",
        "SELECT sum(a) FROM (SELECT 9223372036854775807 a UNION ALL SELECT 1);",
        "SELECT sum(a) FROM (SELECT 9223372036854775807 a UNION ALL SELECT 1.0);",
        "SELECT sum(a) OVER () FROM (SELECT 9223372036854775807 a UNION ALL SELECT 1);",
        "SELECT a, sum(a) OVER (ORDER BY a) FROM (SELECT '9999999999999999999' a UNION ALL SELECT 1);",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
