//! SQLite's trigger-step grammar admits only `SELECT`/`VALUES`/`INSERT`/
//! `REPLACE`/`UPDATE`/`DELETE`/`WITH`-then-`SELECT` statements between `BEGIN`
//! and `END`. graphite used to parse a wider grammar and silently accept
//! several constructs SQLite rejects at prepare time:
//!   * a disallowed leading keyword (`PRAGMA`, `VACUUM`, `CREATE`, …) →
//!     `near "KW": syntax error`,
//!   * a `WITH`-prefixed body `INSERT`/`UPDATE`/`DELETE`/`REPLACE` →
//!     `near "<kw>": syntax error` (the DML keyword, not `WITH`),
//!   * a schema-qualified DML target → `qualified table names are not allowed …`,
//!   * `UPDATE`/`DELETE … RETURNING` → `near "RETURNING": syntax error`,
//!   * `INSERT`/`REPLACE … RETURNING` → `cannot use RETURNING in a trigger`.
//!
//! Because SQLite resolves the trigger's *target* before parsing the body steps,
//! all of these are outranked by a duplicate-name / missing-table / system-table
//! / timing-mismatch error; graphite records the body violation (first in source
//! order) and surfaces it only after those checks. Verified vs sqlite3 3.50.4.
#![cfg(feature = "std")]

use std::process::Command;

fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(sql)
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if !line.is_empty() {
            return line.to_string();
        }
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr.lines() {
        if line.starts_with('^') {
            continue;
        }
        let s = line
            .strip_prefix("Error: in prepare, ")
            .or_else(|| line.strip_prefix("Error: stepping, "))
            .or_else(|| line.strip_prefix("Error: SQL error: "))
            .or_else(|| line.strip_prefix("Error: "))
            .unwrap_or(line);
        let s = s.strip_prefix("error: ").unwrap_or(s);
        let s = s.rsplit_once(" (").map_or(s, |(head, tail)| {
            if tail
                .trim_end_matches(')')
                .chars()
                .all(|c| c.is_ascii_digit())
            {
                head
            } else {
                s
            }
        });
        return s.to_string();
    }
    String::new()
}

fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn same(sql: &str) {
    let g = run(env!("CARGO_BIN_EXE_graphitesql"), sql);
    let s = run("sqlite3", sql);
    assert_eq!(g, s, "mismatch for SQL: {sql}");
}

#[test]
fn trigger_body_disallowed_leading_keyword() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a);";
    for step in [
        "PRAGMA foo",
        "VACUUM",
        "CREATE TABLE x(b)",
        "DROP TABLE t",
        "ALTER TABLE t ADD c",
        "EXPLAIN SELECT 1",
        "SAVEPOINT s",
        "ANALYZE",
        "REINDEX",
        "BEGIN",
        "COMMIT",
        "RELEASE s",
        "ATTACH 'x' AS y",
        "DETACH y",
        "ROLLBACK",
    ] {
        same(&format!(
            "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN {step}; END;"
        ));
    }
}

#[test]
fn trigger_body_with_prefixed_dml() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a);";
    // WITH may only prefix a SELECT/VALUES in a trigger body; a WITH-prefixed DML
    // echoes the DML keyword.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN WITH x AS (SELECT 1) UPDATE t SET a=1; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN WITH x AS (SELECT 1) DELETE FROM t; END;"
    ));
    // … but WITH-then-SELECT / WITH-then-VALUES stays legal (trigger created).
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN WITH x AS (SELECT 1) SELECT * FROM x; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN WITH x AS (SELECT 1) VALUES(1); END;"
    ));
}

#[test]
fn trigger_body_returning() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a);";
    // INSERT/REPLACE RETURNING -> a fixed semantic message.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO t VALUES(1) RETURNING a; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN REPLACE INTO t VALUES(1) RETURNING a; END;"
    ));
    // UPDATE/DELETE RETURNING -> a `near "RETURNING"` syntax error.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a=1 RETURNING a; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN DELETE FROM t RETURNING a; END;"
    ));
    // Regression: a statement-level RETURNING still mutates *and* projects rows.
    same(&format!("{t} INSERT INTO t VALUES(1) RETURNING a;"));
    same("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); UPDATE t SET b=9 RETURNING a, b;");
    same("CREATE TABLE t(a); INSERT INTO t VALUES(1),(2); DELETE FROM t RETURNING a;");
}

#[test]
fn trigger_body_qualified_dml_target() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a);";
    for step in [
        "UPDATE main.t SET a=1",
        "DELETE FROM main.t",
        "INSERT INTO main.t VALUES(1)",
    ] {
        same(&format!(
            "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN {step}; END;"
        ));
    }
    // A qualified table in a body *subquery* stays legal.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a=(SELECT a FROM main.t LIMIT 1); END; INSERT INTO t(a) VALUES(7); SELECT a FROM t;"
    ));
}

#[test]
fn trigger_body_target_resolution_outranks_body() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a);";
    // Missing target -> `no such table: main.nope`, not the body error.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON nope BEGIN PRAGMA foo; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON nope BEGIN INSERT INTO t VALUES(1) RETURNING a; END;"
    ));
    // System-table target.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON sqlite_master BEGIN PRAGMA foo; END;"
    ));
    // Timing mismatch (BEFORE on a view).
    same(&format!(
        "{t} CREATE VIEW v AS SELECT 1 a; CREATE TRIGGER tr BEFORE INSERT ON v BEGIN UPDATE t SET a=1 RETURNING a; END;"
    ));
    // Duplicate name outranks everything.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END; CREATE TRIGGER tr AFTER INSERT ON t BEGIN PRAGMA foo; END;"
    ));
}

#[test]
fn trigger_body_first_violation_wins_in_source_order() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a);";
    // INSERT-RETURNING earlier beats a later PRAGMA, and vice versa.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO t VALUES(1) RETURNING a; PRAGMA foo; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN PRAGMA foo; INSERT INTO t VALUES(1) RETURNING a; END;"
    ));
    // A row-limit earlier beats a later RETURNING.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a=1 ORDER BY a; INSERT INTO t VALUES(1) RETURNING a; END;"
    ));
    // Qualified target vs a PRAGMA in either order.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE main.t SET a=1; PRAGMA foo; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN PRAGMA foo; UPDATE main.t SET a=1; END;"
    ));
}

#[test]
fn trigger_body_valid_steps_still_fire() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a); CREATE TABLE log(m);";
    // A well-formed body builds and fires.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES('hit'); END; INSERT INTO t VALUES(1); SELECT m FROM log;"
    ));
    // A parenthesised SELECT and a plain REPLACE are valid steps.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN (SELECT 1); REPLACE INTO t VALUES(2); END; INSERT INTO t VALUES(1); SELECT count(*) FROM t;"
    ));
}
