//! SQLite accepts an optional transaction *name* after the `TRANSACTION`
//! keyword in `BEGIN`/`COMMIT`/`END` (it is parsed and ignored). The name is an
//! ordinary identifier: a reserved keyword there is a syntax error, and a name
//! without the `TRANSACTION` keyword (`BEGIN foo`) is rejected. Verified against
//! the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;

fn ok(sql: &str) {
    let mut c = Connection::open_memory().unwrap();
    c.execute(sql)
        .unwrap_or_else(|e| panic!("expected {sql:?} to parse, got: {e}"));
}

fn err_near(sql: &str, tok: &str) {
    let mut c = Connection::open_memory().unwrap();
    let e = c.execute(sql).unwrap_err().to_string();
    assert_eq!(
        e,
        format!("SQL error: near \"{tok}\": syntax error"),
        "{sql:?}"
    );
}

#[test]
fn named_transaction_is_accepted() {
    ok("BEGIN TRANSACTION foo");
    ok("BEGIN DEFERRED TRANSACTION foo");
    ok("BEGIN IMMEDIATE TRANSACTION whatever");
    ok("BEGIN EXCLUSIVE TRANSACTION t1");
    // A quoted name (even a keyword) is fine.
    ok("BEGIN TRANSACTION \"select\"");
}

#[test]
fn plain_and_unnamed_forms_still_work() {
    ok("BEGIN");
    ok("BEGIN TRANSACTION");
    ok("BEGIN DEFERRED");
}

#[test]
fn name_requires_the_transaction_keyword() {
    // `BEGIN foo` (no TRANSACTION) is a syntax error, as in sqlite.
    err_near("BEGIN foo", "foo");
}

#[test]
fn reserved_word_is_not_a_valid_transaction_name() {
    err_near("BEGIN TRANSACTION select", "select");
}

#[test]
fn only_one_name_is_allowed() {
    err_near("BEGIN TRANSACTION foo bar", "bar");
}

#[test]
fn commit_accepts_a_name_then_runs() {
    // COMMIT TRANSACTION <name> parses; with no active transaction it then fails
    // at run time exactly like a bare COMMIT would.
    let mut c = Connection::open_memory().unwrap();
    c.execute("BEGIN TRANSACTION work").unwrap();
    c.execute("COMMIT TRANSACTION work").unwrap();
    // END is a COMMIT synonym and takes a name too.
    c.execute("BEGIN TRANSACTION w2").unwrap();
    c.execute("END TRANSACTION w2").unwrap();
}
