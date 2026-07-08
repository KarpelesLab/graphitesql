//! A parenthesized base-window reference (`OVER (base …)`) may extend the base
//! only where the base leaves room: it cannot add a `PARTITION BY`, cannot add an
//! `ORDER BY` when the base already has one, and cannot supply a frame when the
//! base already carries one. SQLite (`sqlite3WindowChain`) rejects each with
//! `cannot override <PARTITION clause|ORDER BY clause|frame specification> of
//! window: <base>`, checked in that order. graphite silently accepted these. The
//! bare `OVER base` form uses the base verbatim and stays valid.

#![cfg(feature = "std")]

use graphitesql::Connection;

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id, x)").unwrap();
    c.execute("INSERT INTO t VALUES(0,1),(1,2),(2,3),(3,2)")
        .unwrap();
    c
}

fn err(c: &Connection, sql: &str) -> String {
    match c.query(sql) {
        Ok(_) => panic!("expected an error for `{sql}`"),
        Err(e) => format!("{e}"),
    }
}

#[test]
fn parenthesized_base_override_is_rejected() {
    let c = setup();
    let framed = "WINDOW w AS (ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)";
    let part_framed = "WINDOW w AS (PARTITION BY x%2 ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)";
    let ordered = "WINDOW w AS (ORDER BY x)";

    // Frame override: the base already has a frame.
    assert!(
        err(&c, &format!("SELECT sum(x) OVER (w) FROM t {framed}"))
            .contains("cannot override frame specification of window: w")
    );
    assert!(err(
        &c,
        &format!(
            "SELECT sum(x) OVER (w ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t {part_framed}"
        )
    )
    .contains("cannot override frame specification of window: w"));
    // ORDER BY override: the base already has an ORDER BY.
    assert!(
        err(
            &c,
            &format!("SELECT sum(x) OVER (w ORDER BY id) FROM t {ordered}")
        )
        .contains("cannot override ORDER BY clause of window: w")
    );
    // PARTITION override always wins (checked first).
    assert!(
        err(
            &c,
            &format!("SELECT sum(x) OVER (w PARTITION BY x) FROM t {ordered}")
        )
        .contains("cannot override PARTITION clause of window: w")
    );
}

#[test]
fn valid_base_extension_and_bare_reference_work() {
    let c = setup();
    // Bare `OVER w` uses even a framed base verbatim.
    let framed = "WINDOW w AS (ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)";
    c.query(&format!("SELECT sum(x) OVER w FROM t {framed} ORDER BY id"))
        .unwrap();
    // Extending an *unframed* base is allowed: add ORDER BY, or add a frame.
    c.query("SELECT sum(x) OVER (w ORDER BY id) FROM t WINDOW w AS (PARTITION BY x%2) ORDER BY id")
        .unwrap();
    c.query(
        "SELECT sum(x) OVER (w ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
         FROM t WINDOW w AS (ORDER BY x) ORDER BY id",
    )
    .unwrap();
    // A plain parenthesized reference to a partition/order (no frame) base.
    c.query("SELECT sum(x) OVER (w) FROM t WINDOW w AS (PARTITION BY x%2 ORDER BY id) ORDER BY id")
        .unwrap();
}
