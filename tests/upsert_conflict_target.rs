//! SQLite validates an `INSERT … ON CONFLICT(target…) DO …` conflict target at
//! prepare time: the target columns must name an actual PRIMARY KEY / UNIQUE
//! constraint or unique index, or it is `ON CONFLICT clause does not match any
//! PRIMARY KEY or UNIQUE constraint`. graphite used to silently accept any
//! target and only (mis)behave at runtime; it now rejects an unmatched target
//! up front, matching sqlite. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

const NO_MATCH: &str = "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint";

/// Run each `;`-separated statement of `setup` on `c` (the in-process API takes
/// one statement at a time).
fn run_setup(c: &mut Connection, setup: &str) {
    for stmt in setup.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        c.execute(stmt).unwrap();
    }
}

/// Run `setup` then `sql` on one connection and return `sql`'s error message
/// (with the in-process `error: ` prefix stripped).
fn err(setup: &str, sql: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    run_setup(&mut c, setup);
    let e = c.execute(sql).unwrap_err().to_string();
    e.trim_start_matches("error: ").to_string()
}

/// Run `setup` then `sql`; assert `sql` succeeds.
fn ok(setup: &str, sql: &str) {
    let mut c = Connection::open_memory().unwrap();
    run_setup(&mut c, setup);
    c.execute(sql)
        .unwrap_or_else(|e| panic!("expected success for {sql:?}, got: {e}"));
}

#[test]
fn no_constraint_on_target_is_rejected() {
    assert_eq!(
        err(
            "CREATE TABLE t(a,b)",
            "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING"
        ),
        NO_MATCH
    );
}

#[test]
fn partial_composite_match_is_rejected() {
    // ON CONFLICT(a) does not match a UNIQUE(a,b): the whole set must match.
    assert_eq!(
        err(
            "CREATE TABLE t(a,b,UNIQUE(a,b))",
            "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING"
        ),
        NO_MATCH
    );
}

#[test]
fn non_unique_index_is_rejected() {
    assert_eq!(
        err(
            "CREATE TABLE t(a,b); CREATE INDEX i ON t(a)",
            "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING"
        ),
        NO_MATCH
    );
}

#[test]
fn partial_unique_index_without_target_where_is_rejected() {
    // A partial unique index only matches a target that itself carries a WHERE.
    assert_eq!(
        err(
            "CREATE TABLE t(a,b); CREATE UNIQUE INDEX i ON t(a) WHERE b>0",
            "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING"
        ),
        NO_MATCH
    );
}

#[test]
fn do_update_with_unmatched_target_is_rejected() {
    assert_eq!(
        err(
            "CREATE TABLE t(a,b)",
            "INSERT INTO t VALUES(1,9) ON CONFLICT(a) DO UPDATE SET b=excluded.b"
        ),
        NO_MATCH
    );
}

#[test]
fn primary_key_target_is_accepted() {
    ok(
        "CREATE TABLE t(a PRIMARY KEY,b)",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING",
    );
    ok(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b)",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING",
    );
    // The rowid alias names the INTEGER PRIMARY KEY.
    ok(
        "CREATE TABLE t(a INTEGER PRIMARY KEY,b)",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(rowid) DO NOTHING",
    );
}

#[test]
fn composite_unique_target_is_order_independent() {
    ok(
        "CREATE TABLE t(a,b,UNIQUE(a,b))",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(b,a) DO NOTHING",
    );
}

#[test]
fn unique_index_target_is_accepted() {
    ok(
        "CREATE TABLE t(a,b); CREATE UNIQUE INDEX i ON t(a)",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING",
    );
}

#[test]
fn partial_unique_index_with_target_where_is_accepted() {
    ok(
        "CREATE TABLE t(a,b); CREATE UNIQUE INDEX i ON t(a) WHERE b>0",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) WHERE b>0 DO NOTHING",
    );
}

#[test]
fn bare_on_conflict_has_no_target_to_match() {
    ok(
        "CREATE TABLE t(a,b)",
        "INSERT INTO t VALUES(1,2) ON CONFLICT DO NOTHING",
    );
}

#[test]
fn matched_do_nothing_still_skips_the_conflicting_row() {
    // The check is purely a prepare-time gate; a valid target still resolves the
    // conflict at runtime (the second insert is silently ignored).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a PRIMARY KEY, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1, 10)").unwrap();
    c.execute("INSERT INTO t VALUES(1, 20) ON CONFLICT(a) DO NOTHING")
        .unwrap();
    let r = c.query("SELECT b FROM t").unwrap();
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let norm = |out: &[u8]| -> String {
        let s = String::from_utf8_lossy(out);
        let first = s.lines().next().unwrap_or("").to_string();
        let first = first
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string();
        // Drop a trailing " (NN)" sqlite error code if present.
        match first.rfind(" (") {
            Some(i) if first.ends_with(')') => first[..i].to_string(),
            _ => first,
        }
    };
    let cases = [
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING",
        "CREATE TABLE t(a,b,UNIQUE(a,b)); INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING",
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(a); INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING",
        "CREATE TABLE t(a,b); CREATE UNIQUE INDEX i ON t(a) WHERE b>0; INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING",
        "CREATE TABLE t(a PRIMARY KEY,b); INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING",
        "CREATE TABLE t(a,b,UNIQUE(a,b)); INSERT INTO t VALUES(1,2) ON CONFLICT(b,a) DO NOTHING",
        "CREATE TABLE t(a,b); CREATE UNIQUE INDEX i ON t(a); INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING",
        "CREATE TABLE t(a,b); CREATE UNIQUE INDEX i ON t(a) WHERE b>0; INSERT INTO t VALUES(1,2) ON CONFLICT(a) WHERE b>0 DO NOTHING",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,9) ON CONFLICT(a) DO UPDATE SET b=excluded.b",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2) ON CONFLICT DO NOTHING",
    ];
    for sql in cases {
        let s = Command::new("sqlite3")
            .arg(":memory:")
            .arg(sql)
            .output()
            .unwrap();
        let gg = Command::new(g).arg(":memory:").arg(sql).output().unwrap();
        assert_eq!(
            norm(&s.stdout) + &norm(&s.stderr),
            norm(&gg.stdout) + &norm(&gg.stderr),
            "mismatch for {sql:?}"
        );
    }
}
