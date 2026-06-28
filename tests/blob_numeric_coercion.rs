//! A blob used in an arithmetic or numeric-function context coerces the way
//! SQLite does: it reads the blob's *bytes as a text string* and applies the
//! ordinary text→number rule. So `x'3132'` (the bytes `0x31 0x32`, i.e. the
//! ASCII text `"12"`) behaves like the number 12 — `abs(x'3132')` is `12.0`,
//! `x'3132' + 0` is `12`, and a blob inside `sum`/`avg`/`total` contributes its
//! parsed value.
//!
//! graphite used to map every blob to `0` in this path (`abs(x'35')` → `0.0`).
//! The fix is a single point — `eval::to_number`'s `Blob` arm — shared by the
//! tree-walker and the VDBE. Affinity application and the strict aggregate
//! integer-vs-real test still leave blobs unconverted (matching SQLite), so a
//! blob keeps BLOB affinity and is non-integer-typed for `sum`.
//!
//! Verified against sqlite3 3.50.4 with the VDBE both on (default) and off.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn one(c: &Connection, sql: &str) -> String {
    let r = c.query(sql).unwrap();
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| format!("{v:?}"))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect::<Vec<_>>()
        .join("|")
}

#[test]
fn blob_coerces_as_text_number() {
    for &vdbe in &[true, false] {
        let c = Connection::open_memory().unwrap();
        c.set_use_vdbe(vdbe);

        // abs reads the bytes as text "5"/"12"/"-12"/"3.5"/"12345".
        assert_eq!(
            one(&c, "SELECT abs(x'35')"),
            "Real(5.0)",
            "abs5 vdbe={vdbe}"
        );
        assert_eq!(
            one(&c, "SELECT abs(x'3132')"),
            "Real(12.0)",
            "abs12 vdbe={vdbe}"
        );
        assert_eq!(
            one(&c, "SELECT abs(x'2D3132')"),
            "Real(12.0)",
            "absNeg vdbe={vdbe}"
        );
        assert_eq!(
            one(&c, "SELECT abs(x'332E35')"),
            "Real(3.5)",
            "absReal vdbe={vdbe}"
        );
        // An all-zero / non-numeric-prefix blob parses to 0.
        assert_eq!(
            one(&c, "SELECT abs(x'00')"),
            "Real(0.0)",
            "absZero vdbe={vdbe}"
        );

        // Arithmetic: blob operands become their text-number value.
        assert_eq!(
            one(&c, "SELECT x'3132' + 0"),
            "Integer(12)",
            "add vdbe={vdbe}"
        );
        assert_eq!(
            one(&c, "SELECT x'3132' + x'3334'"),
            "Integer(46)",
            "addbb vdbe={vdbe}"
        );
        assert_eq!(
            one(&c, "SELECT x'35' * 2"),
            "Integer(10)",
            "mul vdbe={vdbe}"
        );
        assert_eq!(one(&c, "SELECT -x'35'"), "Integer(-5)", "neg vdbe={vdbe}");
        assert_eq!(
            one(&c, "SELECT typeof(x'3132' + 0)"),
            "Text(\"integer\")",
            "typeof vdbe={vdbe}"
        );
        // A leading numeric prefix only (`"1  2"` -> 1), and an empty blob -> 0.
        assert_eq!(
            one(&c, "SELECT x'31202032' + 0"),
            "Integer(1)",
            "prefix vdbe={vdbe}"
        );
        assert_eq!(one(&c, "SELECT x'' + 0"), "Integer(0)", "empty vdbe={vdbe}");

        // Aggregates: a blob contributes its parsed value (and, being
        // non-integer-typed, keeps the sum REAL).
        assert_eq!(
            one(
                &c,
                "SELECT sum(c) FROM (SELECT x'3132' c UNION ALL SELECT 3)"
            ),
            "Real(15.0)",
            "sum vdbe={vdbe}"
        );
        assert_eq!(
            one(
                &c,
                "SELECT avg(c) FROM (SELECT x'3132' c UNION ALL SELECT 4)"
            ),
            "Real(8.0)",
            "avg vdbe={vdbe}"
        );

        // Truthiness also reads the blob as a number.
        assert_eq!(
            one(&c, "SELECT CASE WHEN x'3132' THEN 'y' ELSE 'n' END"),
            "Text(\"y\")",
            "truthy vdbe={vdbe}"
        );
        assert_eq!(
            one(&c, "SELECT CASE WHEN x'00' THEN 'y' ELSE 'n' END"),
            "Text(\"n\")",
            "falsy vdbe={vdbe}"
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
            return et.lines().next().unwrap_or("").to_string();
        }
        s.lines().collect::<Vec<_>>().join("|")
    };
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        "SELECT abs(x'35');",
        "SELECT abs(x'3132');",
        "SELECT abs(x'2D3132');",
        "SELECT abs(x'332E35');",
        "SELECT abs(x'3132333435');",
        "SELECT abs(x'00');",
        "SELECT x'3132' + 0;",
        "SELECT x'3132' + x'3334';",
        "SELECT x'35' * 2;",
        "SELECT -x'35';",
        "SELECT round(x'332E3134',1);",
        "SELECT x'31202032' + 0;",
        "SELECT '12'+x'3334';",
        "SELECT x'' + 0;",
        "SELECT typeof(x'3132'+0);",
        "SELECT sum(c) FROM (SELECT x'3132' c UNION ALL SELECT 3);",
        "SELECT total(c) FROM (SELECT x'3132' c UNION ALL SELECT 3);",
        "SELECT avg(c) FROM (SELECT x'3132' c UNION ALL SELECT 4);",
        "SELECT CASE WHEN x'3132' THEN 'y' ELSE 'n' END;",
        "SELECT CASE WHEN x'00' THEN 'y' ELSE 'n' END;",
        "SELECT x'3132'/x'34';",
        "SELECT x'3132'%5;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
