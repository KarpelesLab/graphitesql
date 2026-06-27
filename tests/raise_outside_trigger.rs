//! `RAISE(ABORT|FAIL|ROLLBACK, msg)` / `RAISE(IGNORE)` is only meaningful inside
//! a trigger program. SQLite parses it everywhere but rejects evaluation outside
//! a trigger with the dedicated message `RAISE() may only be used within a
//! trigger-program`. graphite represents `RAISE(...)` as a canonical `raise(...)`
//! function call that the trigger executor intercepts; outside a trigger it used
//! to fall through to the generic `no such function: raise`. Verified against the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;

/// First error line, with the `error: ` Display prefix stripped.
fn err(sql: &str) -> String {
    let conn = Connection::open_memory().unwrap();
    let e = conn.query(sql).unwrap_err().to_string();
    e.trim_start_matches("error: ").to_string()
}

#[test]
fn raise_outside_a_trigger_is_rejected() {
    let msg = "RAISE() may only be used within a trigger-program";
    assert_eq!(err("SELECT RAISE(ABORT, 'x')"), msg);
    assert_eq!(err("SELECT RAISE(FAIL, 'x')"), msg);
    assert_eq!(err("SELECT RAISE(ROLLBACK, 'x')"), msg);
    assert_eq!(err("SELECT RAISE(IGNORE)"), msg);
    // Lower-case and a non-result-column position resolve the same way.
    assert_eq!(err("SELECT 1 WHERE raise(abort, 'z')"), msg);
}

#[test]
fn raise_inside_a_trigger_still_fires() {
    // The fix only rejects RAISE *outside* a trigger; a real trigger program
    // must keep working. RAISE(FAIL, msg) aborts the firing statement with msg.
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE TABLE t(a)").unwrap();
    conn.execute("CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT RAISE(FAIL, 'boom'); END")
        .unwrap();
    let e = conn
        .execute("INSERT INTO t VALUES(1)")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("boom"),
        "expected trigger RAISE message, got: {e}"
    );
}
