//! Two aggregate name/arity divergences from sqlite, fixed together:
//!
//! * `string_agg(X)` requires its separator — it is registered as an exactly
//!   two-argument aggregate, unlike its `group_concat` alias whose separator is
//!   optional. The one-argument form reports `wrong number of arguments to
//!   function string_agg()` (even with `DISTINCT`, since the arity check runs
//!   before the DISTINCT one).
//! * The JSON group aggregates (`json_group_array`/`json_group_object` and their
//!   `jsonb_` variants) have no scalar counterpart, so a wrong argument count
//!   must reach the aggregate arity guard (`wrong number of arguments`) rather
//!   than fall through to scalar dispatch (`no such function`).
//!
//! Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn graphite_err(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

fn sqlite_err(sql: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .next()
        .unwrap_or("")
        .trim_start_matches("Error: in prepare, ")
        .trim_start_matches("Error: stepping, ")
        .trim_start_matches("Error: ")
        .to_string()
}

#[test]
fn string_agg_requires_separator() {
    let c = Connection::open_memory().unwrap();
    const WRONG: &str = "wrong number of arguments to function string_agg()";
    // One argument (the separator is mandatory) — even with DISTINCT, the arity
    // check fires first.
    for sql in [
        "SELECT string_agg(1)",
        "SELECT string_agg('a')",
        "SELECT string_agg(DISTINCT 'a')",
    ] {
        assert_eq!(graphite_err(&c, sql), WRONG, "for {sql}");
    }
    // The two-argument form is accepted; two args with DISTINCT lands on the
    // DISTINCT message (within the upper bound, so the arity check passes).
    assert!(c.query("SELECT string_agg('a', '-')").is_ok());
    assert_eq!(
        graphite_err(&c, "SELECT string_agg(DISTINCT 'a','-')"),
        "DISTINCT aggregates must have exactly one argument"
    );
}

#[test]
fn json_group_aggregates_are_aggregates_at_any_arity() {
    let c = Connection::open_memory().unwrap();
    // Wrong arity reports the aggregate arity error, not "no such function".
    for (sql, name) in [
        ("SELECT json_group_array()", "json_group_array"),
        ("SELECT json_group_array(1,2)", "json_group_array"),
        ("SELECT jsonb_group_array(1,2)", "jsonb_group_array"),
        ("SELECT json_group_object()", "json_group_object"),
        ("SELECT json_group_object('k')", "json_group_object"),
        ("SELECT jsonb_group_object('k')", "jsonb_group_object"),
    ] {
        assert_eq!(
            graphite_err(&c, sql),
            format!("wrong number of arguments to function {name}()"),
            "for {sql}"
        );
    }
    // The valid arities still produce JSON.
    assert!(c.query("SELECT json_group_array(1)").is_ok());
    assert!(c.query("SELECT json_group_object('k', 1)").is_ok());
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT string_agg(1)",
        "SELECT string_agg('a')",
        "SELECT string_agg(DISTINCT 'a')",
        "SELECT string_agg(DISTINCT 'a','-')",
        "SELECT json_group_array()",
        "SELECT json_group_array(1,2)",
        "SELECT jsonb_group_array(1,2)",
        "SELECT json_group_object()",
        "SELECT json_group_object('k')",
        "SELECT jsonb_group_object('k')",
    ] {
        assert_eq!(graphite_err(&c, sql), sqlite_err(sql), "for {sql}");
    }
}
