//! Track A: `LIKE … ESCAPE`, the `like()` function form, and the `likely`/
//! `unlikely`/`likelihood` optimizer-hint functions. Verified against sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn one(c: &Connection, sql: &str) -> i64 {
    match c.query(sql).unwrap().rows[0][0] {
        Value::Integer(i) => i,
        _ => -999,
    }
}

#[test]
fn escape_semantics() {
    let c = Connection::open_memory().unwrap();
    // '\' escapes the wildcard so it matches literally.
    assert_eq!(one(&c, r#"SELECT 'a_b' LIKE 'a\_b' ESCAPE '\'"#), 1);
    assert_eq!(one(&c, r#"SELECT 'axb' LIKE 'a\_b' ESCAPE '\'"#), 0);
    assert_eq!(one(&c, r#"SELECT '100%' LIKE '100\%' ESCAPE '\'"#), 1);
    assert_eq!(one(&c, r#"SELECT '100x' LIKE '100\%' ESCAPE '\'"#), 0);
    // NOT LIKE ... ESCAPE.
    assert_eq!(one(&c, r#"SELECT 'a_b' NOT LIKE 'a\_b' ESCAPE '\'"#), 0);
    // Plain LIKE still works.
    assert_eq!(one(&c, "SELECT 'abc' LIKE 'a%'"), 1);
    // Optimizer-hint functions are identity.
    assert_eq!(one(&c, "SELECT likely(5)"), 5);
    assert_eq!(one(&c, "SELECT unlikely(7)"), 7);
    assert_eq!(one(&c, "SELECT likelihood(9, 0.5)"), 9);
    // like() function form: like(pattern, text).
    assert_eq!(one(&c, "SELECT like('a%', 'abc')"), 1);
    assert_eq!(one(&c, r#"SELECT like('a\%c', 'a%c', '\')"#), 1);
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let queries = [
        r#"SELECT 'a_b' LIKE 'a\_b' ESCAPE '\'"#,
        r#"SELECT 'a%b' LIKE 'a\%b' ESCAPE '\'"#,
        r#"SELECT 'axb' LIKE 'a_b'"#,
        r#"SELECT count(*) FROM (SELECT 'a_b' AS s UNION ALL SELECT 'axb') WHERE s LIKE 'a\_b' ESCAPE '\'"#,
        r#"SELECT likely(42), unlikely(1), likelihood(3, 0.9)"#,
        r#"SELECT like('%abc%', 'xxabcyy')"#,
    ];
    let c = Connection::open_memory().unwrap();
    let render = |v: &Value| match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    };
    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(q)
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = c.query(q).unwrap().rows[0]
            .iter()
            .map(render)
            .collect::<Vec<_>>()
            .join("|");
        if got != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} LIKE/ESCAPE queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// graphite's `LIKE` is case-insensitive for ASCII only, matching *documented*
/// SQLite (and most builds): a non-ASCII letter is compared case-sensitively, so
/// `'É' LIKE 'é'` is false. (The pinned `sqlite3` 3.50.4 "alt1" oracle is a custom
/// build whose `LIKE` folds full Unicode via the C library's `towlower` — a
/// per-codepoint, locale/build-specific fold that is not replicable byte-for-byte
/// and would diverge graphite from standard SQLite. We intentionally do NOT match
/// it; this test pins the ASCII-only behavior so it is not "fixed" toward the
/// alt1 build by accident. `upper()`/`lower()` are likewise ASCII-only here.)
#[test]
fn like_is_ascii_case_insensitive_only() {
    let c = Connection::open_memory().unwrap();
    let f = |sql: &str| match c.query(sql).unwrap().rows[0][0] {
        Value::Integer(i) => i,
        ref v => panic!("expected int from {sql}, got {v:?}"),
    };
    // ASCII folds (case-insensitive).
    assert_eq!(f("SELECT 'ABC' LIKE 'abc'"), 1);
    assert_eq!(f("SELECT 'abc' LIKE 'ABC'"), 1);
    assert_eq!(f("SELECT 'File.TXT' LIKE 'file.txt'"), 1);
    // Non-ASCII letters are NOT folded.
    assert_eq!(f("SELECT 'É' LIKE 'é'"), 0);
    assert_eq!(f("SELECT 'café' LIKE 'CAFÉ'"), 0);
    assert_eq!(f("SELECT 'Ω' LIKE 'ω'"), 0);
    assert_eq!(f("SELECT 'Ā' LIKE 'ā'"), 0);
    // An exact non-ASCII match still works; wildcards span multibyte chars.
    assert_eq!(f("SELECT 'café' LIKE 'café'"), 1);
    assert_eq!(f("SELECT 'café' LIKE 'caf_'"), 1);
    assert_eq!(f("SELECT 'résumé' LIKE 'r%é'"), 1);
}
