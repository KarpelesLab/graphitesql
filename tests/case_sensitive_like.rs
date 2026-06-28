//! `PRAGMA case_sensitive_like = ON` makes the `LIKE` operator (and the `like()`
//! function) compare ASCII letters case-sensitively; the default folds ASCII
//! case. graphite previously accepted the pragma as a no-op, so `'A' LIKE 'a'`
//! stayed `1` even after turning it on.
//!
//! The flag affects only ASCII (a non-ASCII letter like `É`/`é` never folds,
//! matching SQLite's built-in `LIKE`), `GLOB` is always case-sensitive regardless
//! of it, and the get form (`PRAGMA case_sensitive_like`) returns no rows — it is
//! a write-only toggle. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn one(c: &Connection, sql: &str) -> i64 {
    let r = c.query(sql).unwrap();
    match &r.rows[0][0] {
        graphitesql::Value::Integer(n) => *n,
        v => panic!("expected integer, got {v:?}"),
    }
}

#[test]
fn toggles_like_case_sensitivity() {
    let mut c = Connection::open_memory().unwrap();
    // Default: ASCII-insensitive.
    assert_eq!(one(&c, "SELECT 'A' LIKE 'a'"), 1);
    // Turned on: exact ASCII case.
    c.execute("PRAGMA case_sensitive_like = ON").unwrap();
    assert_eq!(one(&c, "SELECT 'A' LIKE 'a'"), 0);
    assert_eq!(one(&c, "SELECT 'abc' LIKE 'abc'"), 1);
    // The `_` wildcard still matches any single char; only literals are sensitive.
    assert_eq!(one(&c, "SELECT 'aXc' LIKE 'a_c'"), 1);
    assert_eq!(one(&c, "SELECT 'aXc' LIKE 'A_c'"), 0);
    // GLOB is always case-sensitive — the flag does not change it.
    assert_eq!(one(&c, "SELECT 'A' GLOB 'a'"), 0);
    // Turned back off: folding returns.
    c.execute("PRAGMA case_sensitive_like = OFF").unwrap();
    assert_eq!(one(&c, "SELECT 'A' LIKE 'a'"), 1);
}

#[test]
fn applies_to_where_clause_and_like_function() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES ('Apple'),('apple'),('APPLE')")
        .unwrap();
    assert_eq!(one(&c, "SELECT count(*) FROM t WHERE a LIKE 'apple'"), 3);
    c.execute("PRAGMA case_sensitive_like = 1").unwrap();
    assert_eq!(one(&c, "SELECT count(*) FROM t WHERE a LIKE 'apple'"), 1);
    // The two-argument like() function honors the flag too.
    assert_eq!(one(&c, "SELECT like('a','A')"), 0);
    assert_eq!(one(&c, "SELECT like('a','a')"), 1);
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    };
    for sql in [
        "SELECT 'A' LIKE 'a';",
        "PRAGMA case_sensitive_like=1; SELECT 'A' LIKE 'a';",
        "PRAGMA case_sensitive_like=ON; SELECT 'ABC' LIKE 'abc', 'abc' LIKE 'abc';",
        "PRAGMA case_sensitive_like=1; SELECT 'aXc' LIKE 'a_c', 'aXc' LIKE 'A_c';",
        "PRAGMA case_sensitive_like=1; PRAGMA case_sensitive_like=0; SELECT 'A' LIKE 'a';",
        "PRAGMA case_sensitive_like=1; SELECT like('a','A'), like('a','a');",
        "PRAGMA case_sensitive_like=1; SELECT 'A%' LIKE 'a\\%' ESCAPE '\\', 'a%' LIKE 'a\\%' ESCAPE '\\';",
        "PRAGMA case_sensitive_like=1; SELECT 'A' GLOB 'a', 'a' GLOB 'a';",
        // ASCII-only: a non-ASCII letter never folds, both sides agree.
        "PRAGMA case_sensitive_like=1; SELECT 'É' LIKE 'é';",
        // Over a table, exercising the WHERE-clause path.
        "CREATE TABLE t(a); INSERT INTO t VALUES('Apple'),('apple'),('APPLE'); \
         PRAGMA case_sensitive_like=1; SELECT count(*) FROM t WHERE a LIKE 'apple';",
        // The get form yields no rows.
        "PRAGMA case_sensitive_like;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
