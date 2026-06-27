//! An **unknown database qualifier** on a *table reference* is reported the way
//! SQLite reports it: as the referenced object being missing, with the qualifier
//! preserved and the object-kind noun matching the statement — `no such table:
//! bad.t` for a query/DML/`ALTER`/`DROP TABLE`, `no such view:`/`no such index:`/
//! `no such trigger:` for the matching `DROP`. SQLite reserves the bare `unknown
//! database <name>` wording for the `CREATE` forms (`CREATE INDEX bad.i`,
//! `CREATE TRIGGER bad.tr`), whose qualifier names a creation target rather than
//! an object to look up; graphite keeps those. A *known* database with a missing
//! object (`main.nope`, `aux.nope`, `temp.nope`) likewise keeps its qualifier
//! (`no such table: main.nope`) — the qualifier is echoed as written, and the
//! kind-noun still tracks the statement. Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;

/// The library-level error message for a one-shot statement (no CLI framing).
/// `query` is for SELECT, `execute` for everything else.
fn err(setup: &[&str], sql: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for s in setup {
        c.execute(s).unwrap();
    }
    let msg = if sql.trim_start().to_ascii_uppercase().starts_with("SELECT") {
        c.query(sql).unwrap_err().to_string()
    } else {
        c.execute(sql).unwrap_err().to_string()
    };
    // `Error::Error` renders with a leading `error: `; strip it so the assertion
    // reads as SQLite's bare `errmsg` text (what the CLI prints after `Error: `).
    msg.strip_prefix("error: ").unwrap_or(&msg).to_string()
}

#[test]
fn unknown_database_on_a_query_source_is_no_such_table() {
    // FROM and JOIN sources both report the missing table with its qualifier.
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "SELECT * FROM bad.t"),
        "no such table: bad.t"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "SELECT * FROM bad.nope"),
        "no such table: bad.nope"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "SELECT * FROM t JOIN bad.t"),
        "no such table: bad.t"
    );
}

#[test]
fn unknown_database_on_a_dml_target_is_no_such_table() {
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "INSERT INTO bad.t VALUES(1)"),
        "no such table: bad.t"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "UPDATE bad.t SET a=1"),
        "no such table: bad.t"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "DELETE FROM bad.t"),
        "no such table: bad.t"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "ALTER TABLE bad.t RENAME TO u"),
        "no such table: bad.t"
    );
}

#[test]
fn unknown_database_on_a_drop_uses_the_object_kind_noun() {
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "DROP TABLE bad.t"),
        "no such table: bad.t"
    );
    assert_eq!(
        err(&["CREATE VIEW v AS SELECT 1"], "DROP VIEW bad.v"),
        "no such view: bad.v"
    );
    assert_eq!(
        err(
            &["CREATE TABLE t(a)", "CREATE INDEX i ON t(a)"],
            "DROP INDEX bad.i"
        ),
        "no such index: bad.i"
    );
    assert_eq!(
        err(
            &[
                "CREATE TABLE t(a)",
                "CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END",
            ],
            "DROP TRIGGER bad.tr"
        ),
        "no such trigger: bad.tr"
    );
}

#[test]
fn create_keeps_the_unknown_database_wording() {
    // A CREATE's qualifier names where to create the object: an unknown database
    // there is `unknown database <name>`, not a missing-object error.
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "CREATE INDEX bad.i ON t(a)"),
        "unknown database bad"
    );
    assert_eq!(
        err(
            &["CREATE TABLE t(a)"],
            "CREATE TRIGGER bad.tr AFTER INSERT ON t BEGIN SELECT 1; END"
        ),
        "unknown database bad"
    );
}

#[test]
fn known_database_with_a_missing_object_keeps_its_qualifier() {
    // The qualifier names a real database; the object is missing. SQLite echoes
    // the qualifier (`main.nope`), and the kind-noun still tracks the statement.
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "SELECT * FROM main.nope"),
        "no such table: main.nope"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "SELECT * FROM t JOIN main.nope"),
        "no such table: main.nope"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "INSERT INTO main.nope VALUES(1)"),
        "no such table: main.nope"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "UPDATE main.nope SET a=1"),
        "no such table: main.nope"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "DELETE FROM main.nope"),
        "no such table: main.nope"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "ALTER TABLE main.nope RENAME TO u"),
        "no such table: main.nope"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "DROP VIEW main.nope"),
        "no such view: main.nope"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "DROP TRIGGER main.nope"),
        "no such trigger: main.nope"
    );
}

#[test]
fn known_database_qualifier_only_rewrites_the_missing_target() {
    // The rewrite is tied to the target object's own name: a present table whose
    // *column* is missing keeps the bare `no such column` message — the qualifier
    // is not spuriously grafted onto an unrelated error.
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "UPDATE main.t SET b=1"),
        "no such column: b"
    );
    assert_eq!(
        err(&["CREATE TABLE t(a)"], "SELECT nope FROM main.t"),
        "no such column: nope"
    );
}

#[test]
fn attached_database_with_a_missing_object_keeps_its_qualifier() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("CREATE TABLE main.t(a)").unwrap();
    let strip = |m: String| m.strip_prefix("error: ").unwrap_or(&m).to_string();
    assert_eq!(
        strip(c.query("SELECT * FROM aux.nope").unwrap_err().to_string()),
        "no such table: aux.nope"
    );
    assert_eq!(
        strip(
            c.query("SELECT * FROM t JOIN aux.nope")
                .unwrap_err()
                .to_string()
        ),
        "no such table: aux.nope"
    );
    assert_eq!(
        strip(c.execute("DROP TABLE aux.nope").unwrap_err().to_string()),
        "no such table: aux.nope"
    );
}

#[test]
fn valid_qualifiers_and_temp_shadowing_are_unaffected() {
    // A real qualifier still resolves, and an unqualified DML target still lets a
    // temp table shadow main (the `None` resolution path is unchanged).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO main.t VALUES(7)").unwrap();
    assert_eq!(
        c.query("SELECT a FROM main.t").unwrap().rows[0][0],
        graphitesql::Value::Integer(7)
    );

    // A temp table shadows a same-named main table for a bare DML target.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("CREATE TEMP TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM temp.t").unwrap().rows[0][0],
        graphitesql::Value::Integer(1)
    );
    assert_eq!(
        c.query("SELECT count(*) FROM main.t").unwrap().rows[0][0],
        graphitesql::Value::Integer(0)
    );
}
