//! Explicit `COLLATE` operator inside `IN`, `CASE`, and `BETWEEN`, matched to
//! the `sqlite3` CLI (3.50.4). A direct `=` already honored `COLLATE` on either
//! side; these comparison-bearing constructs did not.
//!
//! SQLite's quirks, verified against the CLI:
//!  - `IN`: a single-element list behaves like `x = y` (the element's `COLLATE`
//!    applies), but a multi-element list uses the *left* operand's collation
//!    only — per-element `COLLATE` is ignored there.
//!  - `CASE x WHEN y`: each `WHEN` is an independent comparison, honoring an
//!    explicit `COLLATE` on that `WHEN` (or on the base `x`).
//!  - `BETWEEN`: each bound comparison honors a `COLLATE` on that bound.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn b(c: &Connection, sql: &str) -> i64 {
    match c.query(sql).unwrap().rows.remove(0).remove(0) {
        Value::Integer(i) => i,
        other => panic!("expected integer from {sql}, got {other:?}"),
    }
}

#[test]
fn collate_in_list() {
    let c = Connection::open_memory().unwrap();
    // Single-element list == `x = y`: the element's COLLATE applies.
    assert_eq!(b(&c, "SELECT 'a' IN ('A' COLLATE NOCASE)"), 1);
    assert_eq!(b(&c, "SELECT 'a' COLLATE NOCASE IN ('A')"), 1);
    assert_eq!(b(&c, "SELECT 'a' IN ('A')"), 0);
    // Multi-element list uses the LEFT operand's collation; a per-element COLLATE
    // is ignored (a SQLite quirk).
    assert_eq!(b(&c, "SELECT 'a' IN ('x','A' COLLATE NOCASE)"), 0);
    assert_eq!(b(&c, "SELECT 'a' IN ('A' COLLATE NOCASE,'x')"), 0);
    assert_eq!(b(&c, "SELECT 'a' COLLATE NOCASE IN ('A','x')"), 1);
}

#[test]
fn collate_postfix_after_in() {
    // A COLLATE trailing a closed `IN (…)` construct binds to the whole `IN`
    // expression (SQLite's grammar: `expr ::= expr COLLATE name`, highest
    // precedence). graphite previously rejected this as `near "COLLATE":
    // syntax error`. Since the `IN` result is a 0/1 integer, the trailing
    // collation is a no-op — it never changes the comparison, exactly as in
    // SQLite. The point is that it parses and evaluates rather than erroring.
    let c = Connection::open_memory().unwrap();
    assert_eq!(b(&c, "SELECT 'A' IN ('a','b') COLLATE NOCASE"), 0);
    assert_eq!(b(&c, "SELECT 1 IN (1,2) COLLATE BINARY"), 1);
    assert_eq!(b(&c, "SELECT 'A' NOT IN ('a','b') COLLATE NOCASE"), 1);
    // `IN (SELECT …)` followed by COLLATE parses too.
    assert_eq!(b(&c, "SELECT 'A' IN (SELECT 'a') COLLATE NOCASE"), 0);
    // To actually fold case, COLLATE must sit on the left operand — unchanged.
    assert_eq!(b(&c, "SELECT 'A' COLLATE NOCASE IN ('a','b')"), 1);
}

#[test]
fn collate_case_when() {
    let c = Connection::open_memory().unwrap();
    // An explicit COLLATE on any WHEN applies to that comparison.
    assert_eq!(
        b(
            &c,
            "SELECT CASE 'a' WHEN 'A' COLLATE NOCASE THEN 2 ELSE 0 END"
        ),
        2
    );
    assert_eq!(
        b(
            &c,
            "SELECT CASE 'a' WHEN 'x' THEN 1 WHEN 'A' COLLATE NOCASE THEN 2 ELSE 0 END"
        ),
        2
    );
    // COLLATE on the base operand applies to every WHEN.
    assert_eq!(
        b(
            &c,
            "SELECT CASE 'a' COLLATE NOCASE WHEN 'A' THEN 2 ELSE 0 END"
        ),
        2
    );
    // No COLLATE → BINARY.
    assert_eq!(b(&c, "SELECT CASE 'a' WHEN 'A' THEN 2 ELSE 0 END"), 0);
}

#[test]
fn collate_between_bounds() {
    let c = Connection::open_memory().unwrap();
    // COLLATE on the low bound makes `'Z' >= 'a'` true under NOCASE.
    assert_eq!(b(&c, "SELECT 'Z' BETWEEN 'a' COLLATE NOCASE AND 'z'"), 1);
    // COLLATE on the high bound only affects the `<=` comparison; the `>=` stays
    // BINARY and is already false, so the result is 0 (matching sqlite).
    assert_eq!(b(&c, "SELECT 'Z' BETWEEN 'a' AND 'z' COLLATE NOCASE"), 0);
    // A plain BETWEEN is BINARY.
    assert_eq!(b(&c, "SELECT 'Z' COLLATE NOCASE BETWEEN 'a' AND 'z'"), 1);
}
