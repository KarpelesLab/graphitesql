//! A `CREATE TRIGGER` must have at least one step: an empty `BEGIN END` body is a
//! `near "END": syntax error` in SQLite, but graphite accepted it. Like SQLite's
//! other trigger-body grammar errors it is deferred behind target resolution, so
//! a missing-table error on the trigger's table still outranks it. Verified
//! against the sqlite3 3.50.4 CLI (found by a DDL-strictness probe).

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
fn empty_trigger_body_is_rejected() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // empty body over an existing table → `near "END": syntax error`
    for sql in [
        "CREATE TABLE t(a);CREATE TRIGGER tr BEFORE INSERT ON t BEGIN END;",
        "CREATE TABLE t(a);CREATE TRIGGER tr AFTER UPDATE ON t BEGIN END;",
    ] {
        let (s_out, s_ok) = run("sqlite3", sql);
        let (g_out, g_ok) = run(g, sql);
        assert!(!s_ok && !g_ok, "empty trigger body accepted: `{sql}`");
        assert!(
            s_out.contains("near \"END\"") && g_out.contains("near \"END\""),
            "expected `near \"END\"` for `{sql}`\n  sqlite: {s_out}\n  graphite: {g_out}"
        );
    }
    // empty body over a MISSING table → the missing-table error outranks it
    {
        let sql = "CREATE TRIGGER tr BEFORE INSERT ON nope BEGIN END;";
        let (s_out, s_ok) = run("sqlite3", sql);
        let (g_out, g_ok) = run(g, sql);
        assert!(!s_ok && !g_ok);
        assert!(
            s_out.contains("no such table") && g_out.contains("no such table"),
            "expected `no such table` for `{sql}`\n  sqlite: {s_out}\n  graphite: {g_out}"
        );
    }
    // a non-empty body is still accepted and fires
    let ok = "CREATE TABLE t(a);CREATE TABLE log(x);\
        CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.a); END;\
        INSERT INTO t VALUES(7);SELECT x FROM log;";
    let (s_out, s_ok) = run("sqlite3", ok);
    let (g_out, g_ok) = run(g, ok);
    assert!(s_ok && g_ok, "valid trigger rejected");
    assert_eq!(s_out, g_out);
}
