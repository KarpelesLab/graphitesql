//! A window-free top-level query whose `FROM` is a **`NATURAL` or `USING` join**
//! must resolve its column references against the combined source scope at prepare
//! time, like sqlite — so a reference to a column no source exposes errors even when
//! the join yields no rows. graphite previously bailed on this shape: both
//! `validate_columns_exist` (which declines a `NATURAL`/`USING` join, since the flat
//! `columns` scope would not list a qualified `u.g` of a coalesced pair) and
//! `validate_derived_columns` (which declines any join) left the reference to lazy
//! per-row resolution, so an empty / fully-filtered result silently accepted a bad
//! name.
//!
//! `Executor::validate_join_derived_columns` now also covers this case: each source's
//! columns are resolved exactly as the scan exposes them (a base table via
//! `table_meta`, a view via `try_view`, a derived subquery via `window_source_columns`).
//! A bare name resolves if *any* source exposes it; a qualified `u.g` checks source `u`
//! specifically — so both `t.g` and `u.g` of a coalesced pair resolve. A genuinely
//! *ambiguous* bare name (shared but not coalesced) is left to the separate ambiguity
//! validator — this check only catches missing names. Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(g INTEGER, a TEXT);\n\
    CREATE TABLE u(g INTEGER, b TEXT);\n\
    CREATE TABLE s(g INTEGER, a TEXT);\n\
    CREATE VIEW vt AS SELECT g, a FROM t;\n";

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    c
}

/// The error message with the library's `error: ` framing stripped, so it compares
/// to sqlite's bare prepare-error text.
fn err(c: &Connection, sql: &str) -> String {
    let e = c.query(sql).unwrap_err().to_string();
    e.strip_prefix("error: ").unwrap_or(&e).to_string()
}

#[test]
fn rejects_unknown_column_over_coalesced_join() {
    let c = conn();
    // Tables are empty on purpose: lazy per-row resolution never reaches a row, so
    // this exercises the prepare-time path specifically.
    assert_eq!(
        err(&c, "SELECT zzz FROM t NATURAL JOIN u"),
        "no such column: zzz"
    );
    assert_eq!(
        err(&c, "SELECT t.zzz FROM t NATURAL JOIN u"),
        "no such column: t.zzz"
    );
    assert_eq!(
        err(&c, "SELECT zzz FROM t JOIN u USING (g)"),
        "no such column: zzz"
    );
    // In WHERE, GROUP BY, ORDER BY too.
    assert_eq!(
        err(&c, "SELECT g FROM t JOIN u USING (g) WHERE bad = 1"),
        "no such column: bad"
    );
    assert_eq!(
        err(&c, "SELECT g FROM t JOIN u USING (g) GROUP BY zzz"),
        "no such column: zzz"
    );
    assert_eq!(
        err(&c, "SELECT g FROM t JOIN u USING (g) ORDER BY nope"),
        "no such column: nope"
    );
    // Over a view side, and over a derived/coalesced combination.
    assert_eq!(
        err(&c, "SELECT zzz FROM vt NATURAL JOIN u"),
        "no such column: zzz"
    );
    assert_eq!(
        err(&c, "SELECT bad FROM (SELECT a FROM t) q NATURAL JOIN u"),
        "no such column: bad"
    );
}

#[test]
fn does_not_reject_valid_coalesced_references() {
    let c = conn();
    // The coalesced key resolves bare and under *either* qualifier.
    assert!(c.query("SELECT g FROM t NATURAL JOIN u").is_ok());
    assert!(c.query("SELECT t.g FROM t NATURAL JOIN u").is_ok());
    assert!(c.query("SELECT u.g FROM t NATURAL JOIN u").is_ok());
    // Non-shared columns from each side.
    assert!(c.query("SELECT g, a, b FROM t JOIN u USING (g)").is_ok());
    // A coalesced text column too (t and s share both g and a).
    assert!(c.query("SELECT a FROM t NATURAL JOIN s").is_ok());
    // A view side and a derived side resolve their own columns.
    assert!(c.query("SELECT b FROM vt NATURAL JOIN u").is_ok());
    assert!(c.query("SELECT vt.a, u.b FROM vt JOIN u USING (g)").is_ok());
    assert!(
        c.query("SELECT q.a FROM (SELECT a FROM t) q NATURAL JOIN u")
            .is_ok()
    );
    // A genuinely *ambiguous* bare name (shared but not coalesced) is NOT a missing
    // column — this check stays silent and the ambiguity validator reports it.
    assert_eq!(
        err(&c, "SELECT a FROM t JOIN s USING (g)"),
        "ambiguous column name: a"
    );
}

#[test]
fn matches_sqlite_cli() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        let e = String::from_utf8_lossy(&out.stderr);
        format!("{s}{e}")
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Parse error: ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string()
    };
    for tail in [
        "SELECT zzz FROM t NATURAL JOIN u;",
        "SELECT t.zzz FROM t NATURAL JOIN u;",
        "SELECT zzz FROM t JOIN u USING (g);",
        "SELECT g FROM t JOIN u USING (g) WHERE bad=1;",
        "SELECT g FROM t JOIN u USING (g) GROUP BY zzz;",
        "SELECT g FROM t JOIN u USING (g) ORDER BY nope;",
        "SELECT zzz FROM vt NATURAL JOIN u;",
        "SELECT bad FROM (SELECT a FROM t) q NATURAL JOIN u;",
        "SELECT a FROM t JOIN s USING (g);",
        // Valid queries must still succeed (no false rejection at prepare time).
        "SELECT g FROM t NATURAL JOIN u;",
        "SELECT u.g FROM t NATURAL JOIN u;",
        "SELECT g, a, b FROM t JOIN u USING (g);",
        "SELECT b FROM vt NATURAL JOIN u;",
        "SELECT q.a FROM (SELECT a FROM t) q NATURAL JOIN u;",
    ] {
        let sql = format!("{SETUP} {tail}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {tail}");
    }
}
