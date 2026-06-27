//! A bare `SELECT` step in a trigger body is side-effect-free *except* for a
//! `RAISE(…)`, but SQLite still compiles (resolves) it when the firing statement
//! is prepared — so a FROM-less body `SELECT` referencing a missing column,
//! unknown function, wrong arity, or a bad `NEW`/`OLD` column raises that error
//! when the trigger fires, ahead of the `WHERE` filter and any sibling `RAISE`.
//! graphite used to run the RAISE-only path and silently no-op such a `SELECT`.
//!
//! This covers the FROM-less form (the validation is `eval`-based). A FROM-bearing
//! body `SELECT` (`SELECT * FROM nope`) and an un-taken `CASE` branch's name are
//! still resolved lazily — documented residuals. Verified vs sqlite3 3.50.4.
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
fn body_select_missing_name_errors_on_fire() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a);";
    let i = "INSERT INTO t VALUES(1);";
    // A missing column / unknown function / wrong arity / bad NEW column in a
    // FROM-less body SELECT surfaces when the trigger fires.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT nopecol; END; {i}"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT nosuchfn(1); END; {i}"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT abs(1,2); END; {i}"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT new.nope; END; {i}"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT old.a; END; {i}"
    ));
    // A correlated subquery in the projection is resolved too.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT (SELECT nopecol); END; {i}"
    ));
    // A later step's error: the same resolution still fires.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; SELECT nopecol; END; {i}"
    ));
}

#[test]
fn body_select_resolution_outranks_where_and_raise() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a);";
    let i = "INSERT INTO t VALUES(1);";
    // Resolution happens even when WHERE filters every row out.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT nopecol WHERE 0; END; {i}"
    ));
    // A sibling missing column outranks a RAISE in the same step (SQLite resolves
    // the whole row before any RAISE executes).
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT RAISE(ABORT,'x'), nopecol; END; {i}"
    ));
}

#[test]
fn body_select_error_rolls_back_earlier_step() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a); CREATE TABLE log(x);";
    let i = "INSERT INTO t VALUES(1);";
    // An earlier INSERT side-effect is undone when a later body SELECT errors —
    // and the firing INSERT itself is rolled back.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(1); SELECT nopecol; END; {i} SELECT count(*) FROM log;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT nopecol; END; {i} SELECT count(*) FROM t;"
    ));
}

#[test]
fn body_select_valid_forms_still_fire() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a,b);";
    let i = "INSERT INTO t VALUES(1,2);";
    // Valid projections (NEW refs, expressions, a FROM-less aggregate) are no-ops.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT new.a, new.b, new.a+new.b; END; {i}"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT count(*); END; {i}"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT a FROM t; END; {i}"
    ));
    // RAISE semantics are untouched: conditional IGNORE skips the row, an
    // unconditional ABORT propagates its message.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT RAISE(IGNORE) WHERE new.a<0; END; {i} SELECT count(*) FROM t;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr BEFORE INSERT ON t BEGIN SELECT CASE WHEN new.a<0 THEN RAISE(ABORT,'neg') END; END; INSERT INTO t VALUES(-1,0);"
    ));
}
