//! In `CREATE TABLE`, a table constraint (`PRIMARY KEY(…)`, `UNIQUE(…)`,
//! `CHECK(…)`, `FOREIGN KEY(…)`, or a `CONSTRAINT`-named one) may only appear
//! after all column definitions — once one appears, every following item must
//! also be a table constraint. SQLite rejects a column definition that follows a
//! table constraint with `near "<col>": syntax error`; graphite used to accept
//! it. Verified against the sqlite3 3.50.4 CLI (found by a CHECK/generated-column
//! fuzzer that interleaved constraints between columns).

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> (String, bool) {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    (
        String::from_utf8_lossy(&o.stderr).into_owned() + &String::from_utf8_lossy(&o.stdout),
        o.status.success(),
    )
}

#[test]
fn column_after_table_constraint_is_rejected() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Each should be a syntax error naming the offending column.
    let rejected = [
        ("CREATE TABLE t(a, CHECK(a>0), b);", "near \"b\""),
        (
            "CREATE TABLE t(a, b, CONSTRAINT ck CHECK(a<>b), g AS (a+b));",
            "near \"g\"",
        ),
        ("CREATE TABLE t(a, PRIMARY KEY(a), b);", "near \"b\""),
        ("CREATE TABLE t(a, UNIQUE(a), b INTEGER);", "near \"b\""),
        (
            "CREATE TABLE t(a, b, CHECK(a>0), c, CHECK(b>0));",
            "near \"c\"",
        ),
    ];
    for (sql, near) in rejected {
        let (s_out, s_ok) = run("sqlite3", sql);
        let (g_out, g_ok) = run(g, sql);
        assert!(!s_ok, "sqlite unexpectedly accepted `{sql}`");
        assert!(
            !g_ok,
            "graphite accepted `{sql}` (should be a syntax error)"
        );
        assert!(
            s_out.contains(near) && g_out.contains(near),
            "expected both errors to name {near} for `{sql}`\n  sqlite: {s_out}\n  graphite: {g_out}"
        );
    }
    // Valid: all columns before all constraints — both accept, same rows.
    let accepted = [
        "CREATE TABLE t(a, b, g AS (a+b), CONSTRAINT ck CHECK(a<>b));INSERT INTO t(a,b) VALUES(1,2);SELECT * FROM t;",
        "CREATE TABLE t(a, b, PRIMARY KEY(a), CHECK(a>0), UNIQUE(b));INSERT INTO t VALUES(1,2);SELECT count(*) FROM t;",
    ];
    for sql in accepted {
        let (s_out, s_ok) = run("sqlite3", sql);
        let (g_out, g_ok) = run(g, sql);
        assert!(s_ok && g_ok, "a valid CREATE was rejected: `{sql}`");
        assert_eq!(s_out, g_out, "rows differ for `{sql}`");
    }
}
