//! A window-free top-level query whose `FROM` is an `ON`/cross **join that
//! includes a derived (subquery) source** must resolve its top-level column
//! references against the combined column scope at prepare time, like sqlite — so
//! a reference to a column no source exposes errors even when the join yields no
//! rows. graphite previously bailed on this shape entirely:
//! `validate_columns_exist` declines a non-plain (subquery) source and
//! `validate_derived_columns` declines any join, so the reference was resolved
//! lazily and an empty / fully-filtered result silently accepted the bad name.
//!
//! `Executor::validate_join_derived_columns` closes the gap: each source's columns
//! are resolved exactly as the scan exposes them (a base table via `table_meta`, a
//! view via `try_view`, a derived subquery via `window_source_columns`), and any
//! source that cannot be resolved cleanly bails the whole check (never a false
//! positive). Only a base table carries a `rowid`, so a qualified `x.rowid` over a
//! derived `x` is `no such column` while a bare `rowid` (binding to a base-table
//! source) resolves. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(a INTEGER, b TEXT);\n\
    INSERT INTO t VALUES (1,'x'),(2,'y');\n\
    CREATE TABLE u(c INTEGER, d TEXT);\n\
    INSERT INTO u VALUES (1,'p');\n\
    CREATE VIEW vt AS SELECT a, b FROM t;\n";

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
fn rejects_unknown_column_in_derived_join() {
    let c = conn();
    // A bare missing column, in the projection and in WHERE.
    assert_eq!(
        err(
            &c,
            "SELECT zzz FROM (SELECT a FROM t) x JOIN u ON x.a = u.c"
        ),
        "no such column: zzz"
    );
    assert_eq!(
        err(
            &c,
            "SELECT a FROM u JOIN (SELECT a FROM t) x ON x.a = u.c WHERE zzz = 1"
        ),
        "no such column: zzz"
    );
    // A qualified ref naming the derived source but a column it does not expose.
    assert_eq!(
        err(
            &c,
            "SELECT x.bad FROM (SELECT a FROM t) x JOIN u ON x.a = u.c"
        ),
        "no such column: x.bad"
    );
    // A qualified ref naming the base source but a missing column.
    assert_eq!(
        err(
            &c,
            "SELECT u.bad FROM (SELECT a FROM t) x JOIN u ON x.a = u.c"
        ),
        "no such column: u.bad"
    );
    // A qualifier matching no source is `no such column: q.a` (not `no such table`).
    assert_eq!(
        err(
            &c,
            "SELECT q.a FROM (SELECT a FROM t) x JOIN u ON x.a = u.c"
        ),
        "no such column: q.a"
    );
    // GROUP BY / ORDER BY references are checked too.
    assert_eq!(
        err(
            &c,
            "SELECT a FROM (SELECT a FROM t) x JOIN u ON x.a = u.c GROUP BY zzz"
        ),
        "no such column: zzz"
    );
    assert_eq!(
        err(
            &c,
            "SELECT a FROM (SELECT a FROM t) x JOIN u ON x.a = u.c ORDER BY nope"
        ),
        "no such column: nope"
    );
}

#[test]
fn rowid_and_wildcard_rules() {
    let c = conn();
    // A derived source has no rowid, so a qualified `x.rowid` is a missing column —
    // both graphite (via this check) and sqlite reject it at prepare time.
    assert_eq!(
        err(
            &c,
            "SELECT x.rowid FROM (SELECT a FROM t) x JOIN u ON x.a = u.c"
        ),
        "no such column: x.rowid"
    );
    // A `tbl.*` whose qualifier names no source is `no such table`.
    assert_eq!(
        err(
            &c,
            "SELECT bad.* FROM (SELECT a FROM t) x JOIN u ON x.a = u.c"
        ),
        "no such table: bad"
    );
    // NOTE: a bare `rowid` or a `u.rowid` over a base-table source binds to that
    // table's rowid in sqlite; this check correctly does *not* reject either, but
    // graphite's tree-walker cannot yet *execute* a (bare or qualified) rowid over
    // any join (even a plain base/base one) — a separate, pre-existing gap. So
    // those are intentionally not asserted runnable here.
}

#[test]
fn does_not_reject_valid_references() {
    let c = conn();
    // Bare and qualified references that resolve, across a derived/base join.
    assert!(c
        .query("SELECT a FROM (SELECT a FROM t) x JOIN u ON x.a = u.c")
        .is_ok());
    assert!(c
        .query("SELECT x.a, u.c FROM (SELECT a FROM t) x JOIN u ON x.a = u.c")
        .is_ok());
    // An output alias is in scope for ORDER BY.
    assert!(c
        .query("SELECT a AS z FROM (SELECT a FROM t) x JOIN u ON x.a = u.c ORDER BY z")
        .is_ok());
    // A view joined to a derived table resolves the view's columns.
    assert!(c
        .query("SELECT x.a FROM (SELECT a FROM t) x JOIN vt ON x.a = vt.a WHERE vt.b = 'q'")
        .is_ok());
    // Bare references resolving to either side.
    assert!(c
        .query("SELECT a, c FROM (SELECT a FROM t) x JOIN u ON x.a = u.c WHERE d = 'q'")
        .is_ok());
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
    let setup = "CREATE TABLE t(a INTEGER,b TEXT); CREATE TABLE u(c INTEGER,d TEXT); \
                 CREATE VIEW vt AS SELECT a,b FROM t;";
    for tail in [
        "SELECT zzz FROM (SELECT a FROM t) x JOIN u ON x.a=u.c;",
        "SELECT a FROM u JOIN (SELECT a FROM t) x ON x.a=u.c WHERE zzz=1;",
        "SELECT x.bad FROM (SELECT a FROM t) x JOIN u ON x.a=u.c;",
        "SELECT u.bad FROM (SELECT a FROM t) x JOIN u ON x.a=u.c;",
        "SELECT q.a FROM (SELECT a FROM t) x JOIN u ON x.a=u.c;",
        "SELECT x.rowid FROM (SELECT a FROM t) x JOIN u ON x.a=u.c;",
        "SELECT bad.* FROM (SELECT a FROM t) x JOIN u ON x.a=u.c;",
        "SELECT a FROM (SELECT a FROM t) x JOIN u ON x.a=u.c GROUP BY zzz;",
        "SELECT a FROM (SELECT a FROM t) x JOIN u ON x.a=u.c ORDER BY nope;",
        // Valid queries must still succeed (no false rejection at prepare time).
        "SELECT a FROM (SELECT a FROM t) x JOIN u ON x.a=u.c;",
        "SELECT x.a FROM (SELECT a FROM t) x JOIN vt ON x.a=vt.a WHERE vt.b='q';",
    ] {
        let sql = format!("{setup} {tail}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {tail}");
    }
}
