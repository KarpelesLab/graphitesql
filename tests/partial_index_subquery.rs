//! `CREATE INDEX … WHERE <predicate>` (a partial index) rejects two kinds of
//! predicate before it builds anything: a subquery, and a non-deterministic
//! function. SQLite checks them in a fixed precedence and uses *two distinct*
//! messages for non-determinism depending on where it appears:
//!
//!   1. a non-deterministic *key* expression  → "non-deterministic functions
//!      prohibited in index expressions" (outranks a WHERE subquery)
//!   2. a subquery in the WHERE predicate      → "subqueries prohibited in
//!      partial index WHERE clauses" (outranks a non-deterministic WHERE)
//!   3. a non-deterministic function in WHERE  → "non-deterministic functions
//!      prohibited in partial index WHERE clauses"
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;

fn err(sql: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

const SUBQUERY: &str = "subqueries prohibited in partial index WHERE clauses";
const KEY_NONDET: &str = "non-deterministic functions prohibited in index expressions";
const WHERE_NONDET: &str = "non-deterministic functions prohibited in partial index WHERE clauses";

#[test]
fn subquery_in_where_is_rejected() {
    assert_eq!(
        err("CREATE INDEX i ON t(a) WHERE b IN (SELECT 1)"),
        SUBQUERY
    );
    assert_eq!(
        err("CREATE INDEX i ON t(a) WHERE b IN (SELECT 1 FROM t)"),
        SUBQUERY
    );
    assert_eq!(
        err("CREATE INDEX i ON t(a) WHERE b = (SELECT max(a) FROM t)"),
        SUBQUERY
    );
    assert_eq!(
        err("CREATE INDEX i ON t(a) WHERE EXISTS (SELECT 1)"),
        SUBQUERY
    );
}

#[test]
fn subquery_error_precedes_unknown_column_and_where_nondeterminism() {
    // The subquery is reported even when the predicate also references an unknown
    // column or wraps a non-deterministic function inside the subquery.
    assert_eq!(
        err("CREATE INDEX i ON t(a) WHERE zzz IN (SELECT 1)"),
        SUBQUERY
    );
    assert_eq!(
        err("CREATE INDEX i ON t(a) WHERE b IN (SELECT random())"),
        SUBQUERY
    );
}

#[test]
fn nondeterministic_key_outranks_a_where_subquery() {
    assert_eq!(
        err("CREATE INDEX i ON t(random()) WHERE b IN (SELECT 1)"),
        KEY_NONDET
    );
    assert_eq!(err("CREATE INDEX i ON t(random()) WHERE b > 0"), KEY_NONDET);
}

#[test]
fn nondeterministic_where_uses_its_own_message() {
    assert_eq!(
        err("CREATE INDEX i ON t(a) WHERE random() > 0"),
        WHERE_NONDET
    );
}

#[test]
fn deterministic_partial_index_is_accepted() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE INDEX i1 ON t(a) WHERE b > 0").unwrap();
    c.execute("CREATE INDEX i2 ON t(a) WHERE abs(b) > 0")
        .unwrap();
}
