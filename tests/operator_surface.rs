//! Differential testing of operator / expression semantics against the real
//! `sqlite3` CLI (the 3.50.4 oracle on PATH): the `||` concatenation operator
//! across every storage-class pair, comparison/affinity, `IS`/`IS NOT`, bitwise
//! ops (incl. extreme shifts that must not panic), `%`/`/` overflow and
//! divide-by-zero, unary `+`/`-`/`NOT`, and `LIKE`/`GLOB` edge cases (empty
//! pattern, unterminated class, reversed range, a literal leading `]`, the
//! operator `ESCAPE` form).
//!
//! Every assertion hard-codes the value observed from `sqlite3`. Blob bytes and
//! storage classes are captured with `hex()` / `quote()` / `typeof()` so the
//! comparison is exact.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3(sql: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn render(result: &graphitesql::QueryResult) -> String {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Text(s) => s.clone(),
                    Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                    Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Assert graphitesql produces byte-for-byte the same rendered output as the
/// `sqlite3` CLI for each query.
fn assert_matches(g: &mut Connection, queries: &[&str]) {
    for q in queries {
        let want = sqlite3(&format!("{q};"));
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "operator query diverged: {q}");
    }
}

#[test]
fn operator_surface_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let mut g = Connection::open_memory().unwrap();

    // ---- `||` concatenation: bytes via hex() agree for every pair, and the
    // storage class (typeof) agrees whenever the result is valid UTF-8. The two
    // blobs `x'00'||x'ff'` would be non-UTF-8 text in sqlite; graphitesql models
    // text as UTF-8 so returns those bytes as a blob — bytes still match via
    // hex(), which is what these assertions check.
    assert_matches(
        &mut g,
        &[
            "SELECT hex(x'00' || x'ff')",     // 00FF — the previously-mangled case
            "SELECT hex(x'01' || 'a')",       // 0161
            "SELECT hex('a' || x'01')",       // 6101
            "SELECT hex(x'3132' || x'3334')", // 31323334
            "SELECT hex(x'c3' || x'28')",     // C328 (invalid utf8, bytes preserved)
            "SELECT hex(x'01' || 1)",         // 0131
            "SELECT hex(1 || x'01')",         // 3101
            "SELECT hex(x'01' || 1.5)",       // 01312E35
            // UTF-8-valid results keep text storage class, matching sqlite.
            "SELECT quote(1 || 2), typeof(1 || 2)",
            "SELECT quote(1.5 || 'x'), typeof(1.5 || 'x')",
            "SELECT quote('a' || 1), typeof('a' || 1)",
            "SELECT quote('é' || x'41'), typeof('é' || x'41')",
            // NULL is contagious through ||.
            "SELECT quote(x'01' || NULL), quote(NULL || 'a'), quote(1 || NULL)",
        ],
    );

    // ---- comparison / affinity across storage classes.
    assert_matches(
        &mut g,
        &[
            "SELECT 1 < 'a', 'a' < 1, 1 = '1', '1' = 1, 1.0 = '1'",
            "SELECT x'31' = '1', x'31' = 1, 'abc' < x'00', x'00' < 'abc'",
            "SELECT 10 < 9.5, '10' < '9', '5' < '40'",
        ],
    );

    // ---- IS / IS NOT: numeric equality bridges int/real, but never text/blob.
    assert_matches(
        &mut g,
        &[
            "SELECT 1.0 IS 1, 1 IS 1.0, 1.5 IS 1, 0.0 IS 0",
            "SELECT x'31' IS '1', 'a' IS 'a', NULL IS NULL, 1 IS NOT 1.0",
        ],
    );

    // ---- bitwise ops on reals (truncate to int), text, NULL.
    assert_matches(
        &mut g,
        &[
            "SELECT 5 & 3, 5 | 3, 1 << 4, 256 >> 2, ~5",
            "SELECT 2.9 & 1, 2.9 | 1, ~2.9, '3' & 1, 'abc' & 1, ~'5'",
            "SELECT 5 & NULL, NULL | 3, ~NULL, NULL << 2",
            // Shift edges, including extreme magnitudes that must not panic.
            "SELECT 1 << 63, 1 << 64, 1 << 100, -1 << 1, 1 >> 100, -8 >> 1, -8 >> 100",
            "SELECT 255 >> -1, 1 << 9223372036854775807, 1 >> 9223372036854775807",
            "SELECT 1 << -9223372036854775808, -1 >> -9223372036854775808, 1 >> -9223372036854775808",
        ],
    );

    // ---- `%` and `/` with zero, reals, negatives, and i64::MIN overflow.
    assert_matches(
        &mut g,
        &[
            "SELECT 5 % 0, 5 / 0, 5.0 % 0, 5.0 / 0, 5 % 0.0",
            "SELECT -7 % 3, 7 % -3, -7 % -3, -7 / 3, 7 / -2",
            // i64::MIN / -1 overflows -> sqlite promotes to real; remainder is 0.
            "SELECT (-9223372036854775807-1) / -1, typeof((-9223372036854775807-1) / -1)",
            "SELECT (-9223372036854775807-1) % -1",
            "SELECT 9223372036854775807 + 1, -9223372036854775808 - 1, 9223372036854775807 * 2",
        ],
    );

    // ---- unary +/-/NOT on text, blob, NULL, and the negate overflow.
    assert_matches(
        &mut g,
        &[
            "SELECT -'5', +'abc', -'3.5', -'abc', +NULL, -NULL",
            "SELECT NOT 0, NOT 1, NOT 'abc', NOT '5', NOT NULL, NOT x'00'",
            // -(i64::MIN) overflows -> real (9.22e18).
            "SELECT -(-9223372036854775808), typeof(-(-9223372036854775808))",
        ],
    );

    // ---- LIKE edges: no character classes (the `[...]` is literal), empty
    // pattern, ESCAPE for the operator form, case-insensitive ASCII.
    assert_matches(
        &mut g,
        &[
            "SELECT '' LIKE '', 'a' LIKE '', '' LIKE '%', 'abc' LIKE 'a[bc]'",
            "SELECT 'abc' LIKE 'A%' ESCAPE '\\', 'a%c' LIKE 'a\\%c' ESCAPE '\\'",
            "SELECT 'a_c' LIKE 'a\\_c' ESCAPE '\\', 'abc' LIKE 'a\\_c' ESCAPE '\\'",
        ],
    );

    // ---- GLOB edges: empty pattern, unterminated/reversed class, and a literal
    // leading `]` in a class (`[]...]` / `[^]...]`) — the previously-buggy case.
    assert_matches(
        &mut g,
        &[
            "SELECT 'a' GLOB '', '' GLOB '', '' GLOB '*'",
            "SELECT 'abc' GLOB 'a[b-', 'abc' GLOB 'a[', 'a' GLOB '[z-a]', 'm' GLOB '[z-a]'",
            // Literal leading `]`.
            "SELECT 'a]c' GLOB 'a[]]c', ']' GLOB '[]]', 'x' GLOB '[^]]', ']' GLOB '[^]]'",
            "SELECT ']' GLOB '[]a]', 'a' GLOB '[]a]', 'b' GLOB '[]a]'",
            "SELECT ']' GLOB '[^]a]', 'b' GLOB '[^]a]', 'a' GLOB '[^]a]'",
            "SELECT '-' GLOB '[a-]', 'a' GLOB '[a-]', ']' GLOB '[]-_]', '^' GLOB '[]-_]'",
            "SELECT 'a-c' GLOB 'a[!-]c', 'a!c' GLOB 'a[!-]c'",
        ],
    );

    // ---- BETWEEN / IN affinity (not collation).
    assert_matches(
        &mut g,
        &[
            "SELECT 5 BETWEEN '1' AND '9', '5' BETWEEN 1 AND 9, 1 BETWEEN 0 AND 'a'",
            "SELECT 5 IN ('5'), '5' IN (5), x'05' IN (5)",
        ],
    );
}
