//! `CREATE TABLE`/`CREATE VIEW`/`CREATE INDEX` name collisions. SQLite keeps
//! tables, views and indexes in one namespace (triggers are separate) and names
//! the *existing* object's kind in the error:
//!
//! - a `CREATE TABLE`/`CREATE VIEW` over an existing table → `table X already
//!   exists`; over a view → `view X already exists`; over an index → `there is
//!   already an index named X`.
//! - a `CREATE INDEX` over an existing table or view → `there is already a table
//!   named X` (it says "table" even for a view); over an index → `index X
//!   already exists`.
//!
//! graphite previously reported every table/view collision as `table X already
//! exists` and silently *allowed* a `CREATE TABLE`/`CREATE INDEX` whose name was
//! already taken by a view/index (a duplicate-name hazard). Verified against the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run `setup` statements (each must succeed), then `bad` (which must fail), and
/// return the failure message with the outer Display tag stripped.
fn err_after(setup: &[&str], bad: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for s in setup {
        c.execute(s).unwrap();
    }
    c.execute(bad)
        .unwrap_err()
        .to_string()
        .trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn create_table_or_view_names_the_existing_kind() {
    // existing table.
    assert_eq!(
        err_after(&["CREATE TABLE x(a)"], "CREATE TABLE x(b)"),
        "table x already exists"
    );
    assert_eq!(
        err_after(&["CREATE TABLE x(a)"], "CREATE VIEW x AS SELECT 1"),
        "table x already exists"
    );
    // existing view.
    assert_eq!(
        err_after(&["CREATE VIEW v AS SELECT 1"], "CREATE VIEW v AS SELECT 2"),
        "view v already exists"
    );
    assert_eq!(
        err_after(&["CREATE VIEW v AS SELECT 1"], "CREATE TABLE v(a)"),
        "view v already exists"
    );
    // existing index.
    assert_eq!(
        err_after(
            &["CREATE TABLE t(c)", "CREATE INDEX ix ON t(c)"],
            "CREATE TABLE ix(a)"
        ),
        "there is already an index named ix"
    );
    assert_eq!(
        err_after(
            &["CREATE TABLE t(c)", "CREATE INDEX ix ON t(c)"],
            "CREATE VIEW ix AS SELECT 1"
        ),
        "there is already an index named ix"
    );
}

#[test]
fn create_index_collides_with_a_table_or_view() {
    assert_eq!(
        err_after(
            &["CREATE TABLE t(c)", "CREATE TABLE foo(a)"],
            "CREATE INDEX foo ON t(c)"
        ),
        "there is already a table named foo"
    );
    // Even a *view* is reported as "table" by CREATE INDEX, matching SQLite.
    assert_eq!(
        err_after(
            &["CREATE TABLE t(c)", "CREATE VIEW vw AS SELECT 1"],
            "CREATE INDEX vw ON t(c)"
        ),
        "there is already a table named vw"
    );
    // A duplicate index keeps its own wording.
    assert_eq!(
        err_after(
            &["CREATE TABLE t(c)", "CREATE INDEX ix ON t(c)"],
            "CREATE INDEX ix ON t(c)"
        ),
        "index ix already exists"
    );
}

#[test]
fn if_not_exists_and_distinct_names_are_unaffected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(c)").unwrap();
    c.execute("CREATE VIEW v AS SELECT 1").unwrap();
    c.execute("CREATE INDEX ix ON t(c)").unwrap();
    // IF NOT EXISTS over the same kind is a no-op, not an error.
    c.execute("CREATE TABLE IF NOT EXISTS t(z)").unwrap();
    c.execute("CREATE VIEW IF NOT EXISTS v AS SELECT 2")
        .unwrap();
    c.execute("CREATE INDEX IF NOT EXISTS ix ON t(c)").unwrap();
    // Distinct names coexist.
    c.execute("CREATE TABLE t2(c)").unwrap();
    c.execute("CREATE VIEW v2 AS SELECT 1").unwrap();
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
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    let p = "CREATE TABLE t(c); ";
    for sql in [
        "CREATE TABLE x(a); CREATE TABLE x(b)",
        "CREATE TABLE x(a); CREATE VIEW x AS SELECT 1",
        "CREATE VIEW v AS SELECT 1; CREATE VIEW v AS SELECT 2",
        "CREATE VIEW v AS SELECT 1; CREATE TABLE v(a)",
        &format!("{p}CREATE INDEX ix ON t(c); CREATE TABLE ix(a)"),
        &format!("{p}CREATE INDEX ix ON t(c); CREATE VIEW ix AS SELECT 1"),
        &format!("{p}CREATE TABLE foo(a); CREATE INDEX foo ON t(c)"),
        &format!("{p}CREATE VIEW vw AS SELECT 1; CREATE INDEX vw ON t(c)"),
        &format!("{p}CREATE INDEX ix ON t(c); CREATE INDEX ix ON t(c)"),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
