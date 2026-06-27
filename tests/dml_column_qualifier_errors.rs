//! A three-part `schema.table.column` reference in an `UPDATE`/`DELETE`
//! `WHERE`/`SET`/`RETURNING` is validated against the *target's real database*,
//! exactly like SQLite. The qualifier must name the database the write actually
//! lands in — and that database is resolved before the target is swapped into
//! the active `main` slot, so an unqualified write to a **temp** table (which
//! shadows main) validates against `temp`, not `main`. Matched to the `sqlite3`
//! CLI (3.50.4).
//!
//! These fire at prepare time (over an empty table, before any row is touched),
//! the same as the SELECT-side check in `column_qualifier_errors.rs`. `RETURNING`
//! is stricter than `WHERE`/`SET`: SQLite rejects *any* schema-qualified column
//! there, even a correct one.

#![cfg(feature = "std")]

use graphitesql::Connection;

/// Run `sql` after `setup`; return the `no such column` message (no `error: `
/// frame), or `"<ok>"` if it succeeded.
fn run(setup: &[&str], sql: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for s in setup {
        c.execute(s).unwrap();
    }
    match c.execute(sql) {
        Ok(_) => "<ok>".to_string(),
        Err(e) => {
            let m = e.to_string();
            m.strip_prefix("error: ").unwrap_or(&m).to_string()
        }
    }
}

#[test]
fn wrong_qualifier_in_where_and_set_is_no_such_column() {
    let setup = &["CREATE TABLE t(a, b)"];
    // `bad` names no database; `temp` names no in-scope database for a main table.
    assert_eq!(
        run(setup, "UPDATE t SET a = 9 WHERE bad.t.a = 1"),
        "no such column: bad.t.a"
    );
    assert_eq!(
        run(setup, "UPDATE t SET a = 9 WHERE temp.t.a = 1"),
        "no such column: temp.t.a"
    );
    assert_eq!(
        run(setup, "UPDATE t SET b = bad.t.a"),
        "no such column: bad.t.a"
    );
    assert_eq!(
        run(setup, "DELETE FROM t WHERE bad.t.a = 1"),
        "no such column: bad.t.a"
    );
}

#[test]
fn correct_main_qualifier_resolves_like_the_bare_column() {
    let setup = &["CREATE TABLE t(a, b)", "INSERT INTO t VALUES(1, 2)"];
    assert_eq!(run(setup, "UPDATE t SET a = 9 WHERE main.t.a = 1"), "<ok>");
    assert_eq!(run(setup, "UPDATE t SET b = main.t.a"), "<ok>");
    assert_eq!(run(setup, "DELETE FROM t WHERE main.t.a = 1"), "<ok>");
    // A `main`-qualified rowid resolves too.
    assert_eq!(
        run(setup, "UPDATE t SET a = 9 WHERE main.t.rowid = 1"),
        "<ok>"
    );
}

#[test]
fn unqualified_temp_target_validates_against_temp_not_main() {
    // The regression this guards: an unqualified `UPDATE`/`DELETE` on a temp
    // table is swapped into the active `main` slot before execution, so a naive
    // resolver would mislabel the target as `main`. The qualifier must validate
    // against `temp`.
    let setup = &["CREATE TEMP TABLE t(a, b)", "INSERT INTO t VALUES(1, 2)"];
    assert_eq!(run(setup, "UPDATE t SET a = 9 WHERE temp.t.a = 1"), "<ok>");
    assert_eq!(run(setup, "UPDATE t SET b = temp.t.a"), "<ok>");
    assert_eq!(run(setup, "DELETE FROM t WHERE temp.t.a = 1"), "<ok>");
    // ...and a `main.` qualifier on that temp target is rejected.
    assert_eq!(
        run(setup, "UPDATE t SET a = 9 WHERE main.t.a = 1"),
        "no such column: main.t.a"
    );
    assert_eq!(
        run(setup, "DELETE FROM t WHERE main.t.a = 1"),
        "no such column: main.t.a"
    );
}

#[test]
fn returning_rejects_any_schema_qualified_column() {
    let setup = &["CREATE TABLE t(a, b)", "INSERT INTO t VALUES(1, 2)"];
    // Even the correct database part is rejected in RETURNING.
    assert_eq!(
        run(setup, "UPDATE t SET a = 5 RETURNING main.t.a"),
        "no such column: main.t.a"
    );
    // A two-part `t.a` and a bare `b` are fine.
    assert_eq!(run(setup, "UPDATE t SET a = 5 RETURNING t.a"), "<ok>");
    assert_eq!(run(setup, "UPDATE t SET a = 5 RETURNING b"), "<ok>");
}
