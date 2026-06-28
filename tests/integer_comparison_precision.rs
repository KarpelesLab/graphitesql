//! Two `i64` values must compare exactly, never through a lossy `f64`
//! round-trip. graphite's value comparison (`cmp_values`, the routine behind
//! `=`/`<`/`>`, `ORDER BY`, index seeks, `DISTINCT`, `GROUP BY`, and `IN`) used
//! to coerce both numeric operands to `f64` and compare those — so any two
//! integers above 2^53 that share an `f64` rounding (e.g. 10^16 and 10^16 + 1)
//! wrongly read as equal, silently corrupting equality, ordering, de-duplication
//! and grouping.
//!
//! The fix compares two integers as `i64`, two reals as `f64`, and a mixed
//! integer/real pair with SQLite's exact `sqlite3IntFloatCompare`. Verified
//! against sqlite3 3.50.4 with the VDBE both on (default) and off.

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
fn large_integers_compare_exactly() {
    for &vdbe in &[true, false] {
        let mut c = Connection::open_memory().unwrap();
        c.set_use_vdbe(vdbe);

        // The two integers differ by 1 and share an f64 (both round to 1e16).
        assert_eq!(
            one(&c, "SELECT 10000000000000000 = 10000000000000001"),
            one(&c, "SELECT 0"),
            "equality, vdbe={vdbe}"
        );
        assert_eq!(
            one(&c, "SELECT 10000000000000000 < 10000000000000001"),
            one(&c, "SELECT 1"),
            "less-than, vdbe={vdbe}"
        );
        assert_eq!(
            one(&c, "SELECT 9223372036854775807 = 9223372036854775806"),
            one(&c, "SELECT 0"),
            "i64::MAX neighbours, vdbe={vdbe}"
        );
        assert_eq!(
            one(&c, "SELECT 9007199254740993 = 9007199254740992"),
            one(&c, "SELECT 0"),
            "just above 2^53, vdbe={vdbe}"
        );

        // ORDER BY must keep the two distinct and in the right order.
        assert_eq!(
            one(
                &c,
                "SELECT x FROM (SELECT 10000000000000001 x UNION ALL SELECT 10000000000000000) ORDER BY x"
            ),
            "Integer(10000000000000000)|Integer(10000000000000001)",
            "order by, vdbe={vdbe}"
        );
        // DISTINCT must not merge them.
        assert_eq!(
            one(
                &c,
                "SELECT count(DISTINCT x) FROM (SELECT 10000000000000001 x UNION ALL SELECT 10000000000000000)"
            ),
            "Integer(2)",
            "distinct, vdbe={vdbe}"
        );
        // GROUP BY must keep two groups.
        assert_eq!(
            one(
                &c,
                "SELECT count(*) FROM (SELECT x FROM (SELECT 10000000000000001 x UNION ALL SELECT 10000000000000000) GROUP BY x)"
            ),
            "Integer(2)",
            "group by, vdbe={vdbe}"
        );
        // IN must not match a distinct neighbour.
        assert_eq!(
            one(&c, "SELECT 10000000000000001 IN (10000000000000000)"),
            one(&c, "SELECT 0"),
            "in, vdbe={vdbe}"
        );

        // A mixed integer/real comparison stays exact at the i64 boundary.
        assert_eq!(
            one(&c, "SELECT 10000000000000001 = 1e16"),
            one(&c, "SELECT 0"),
            "int-vs-real distinct, vdbe={vdbe}"
        );
        assert_eq!(
            one(&c, "SELECT 10000000000000000 = 1e16"),
            one(&c, "SELECT 1"),
            "int-vs-real equal, vdbe={vdbe}"
        );

        // Index-seek path (INTEGER PRIMARY KEY) must filter exactly.
        c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY)").unwrap();
        c.execute(
            "INSERT INTO t VALUES(10000000000000000),(10000000000000001),(10000000000000002)",
        )
        .unwrap();
        assert_eq!(
            one(&c, "SELECT a FROM t WHERE a=10000000000000001"),
            "Integer(10000000000000001)",
            "index seek, vdbe={vdbe}"
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
        "SELECT 10000000000000000 = 10000000000000001;",
        "SELECT 10000000000000000 < 10000000000000001;",
        "SELECT 9223372036854775807 = 9223372036854775806;",
        "SELECT 9223372036854775807 > 9223372036854775806;",
        "SELECT 9007199254740993 = 9007199254740992;",
        "SELECT x FROM (SELECT 10000000000000001 x UNION ALL SELECT 10000000000000000) ORDER BY x;",
        "SELECT count(DISTINCT x) FROM (SELECT 10000000000000001 x UNION ALL SELECT 10000000000000000);",
        "SELECT x, count(*) FROM (SELECT 10000000000000001 x UNION ALL SELECT 10000000000000000) GROUP BY x;",
        "SELECT 10000000000000001 IN (10000000000000000);",
        "SELECT 10000000000000001 = 1e16;",
        "SELECT 10000000000000000 = 1e16;",
        "SELECT 9223372036854775807 = 9223372036854775807.0;",
        "SELECT 9223372036854775807 < 9223372036854775808.0;",
        "SELECT -9223372036854775808 = -9223372036854775808.0;",
        "SELECT 4503599627370497 < 4503599627370497.5;",
        "CREATE TABLE t(a INTEGER PRIMARY KEY); INSERT INTO t VALUES(10000000000000000),(10000000000000001); SELECT a FROM t WHERE a=10000000000000001;",
        "CREATE TABLE t(a INTEGER PRIMARY KEY); INSERT INTO t VALUES(10000000000000000),(10000000000000001); SELECT a FROM t ORDER BY a DESC;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
