//! `ALTER TABLE … DROP COLUMN` rejection when a *dependent view or trigger*
//! references the dropped column.
//!
//! SQLite, after the structural table/index checks, re-validates every other
//! schema object (in `sqlite_schema` rowid order) against the post-drop table
//! and rejects the first one that no longer resolves, with
//! `error in {view|trigger} NAME after drop column: no such column: <ref>`,
//! echoing the offending reference exactly (`c`, `t.c`, `x.c`, `NEW.c`, `OLD.c`).
//! Only value/expression-position references count — assignment *targets*
//! (`SET col=`, an `INSERT INTO t(col)` column list, a trigger's `UPDATE OF col`)
//! do not trigger rejection. graphite used to leave the now-broken object in the
//! schema. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn err(c: &mut Connection, sql: &str) -> String {
    c.execute(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn a_view_referencing_the_dropped_column_names_the_view_and_ref() {
    // (setup-before-t, expected-message). `t(a, b, c)`; we drop `c`.
    let cases: &[(&str, &str)] = &[
        (
            "CREATE VIEW v AS SELECT c FROM t",
            "error in view v after drop column: no such column: c",
        ),
        (
            "CREATE VIEW v AS SELECT t.c FROM t",
            "error in view v after drop column: no such column: t.c",
        ),
        (
            "CREATE VIEW v AS SELECT x.c FROM t x",
            "error in view v after drop column: no such column: x.c",
        ),
        (
            "CREATE VIEW v AS SELECT a FROM t WHERE c > 0",
            "error in view v after drop column: no such column: c",
        ),
        (
            "CREATE VIEW v AS SELECT a, c AS keep FROM t",
            "error in view v after drop column: no such column: c",
        ),
    ];
    for (setup, msg) in cases {
        let mut c = Connection::open_memory().unwrap();
        c.execute("CREATE TABLE t(a, b, c)").unwrap();
        c.execute(setup).unwrap();
        assert_eq!(
            err(&mut c, "ALTER TABLE t DROP COLUMN c"),
            *msg,
            "for {setup}"
        );
    }
}

#[test]
fn a_trigger_referencing_the_dropped_column_names_the_trigger_and_ref() {
    let cases: &[(&str, &str)] = &[
        (
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.c); END",
            "error in trigger tr after drop column: no such column: NEW.c",
        ),
        (
            "CREATE TRIGGER tr AFTER DELETE ON t BEGIN INSERT INTO log VALUES(OLD.c); END",
            "error in trigger tr after drop column: no such column: OLD.c",
        ),
        (
            "CREATE TRIGGER tr AFTER INSERT ON t WHEN NEW.c > 0 BEGIN INSERT INTO log VALUES(1); END",
            "error in trigger tr after drop column: no such column: NEW.c",
        ),
        (
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN DELETE FROM t WHERE c = 1; END",
            "error in trigger tr after drop column: no such column: c",
        ),
        (
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET b = c + 1; END",
            "error in trigger tr after drop column: no such column: c",
        ),
        (
            "CREATE TRIGGER tr AFTER INSERT ON s BEGIN INSERT INTO log SELECT c FROM t; END",
            "error in trigger tr after drop column: no such column: c",
        ),
        // `UPDATE … SET … FROM t`: the joined `FROM t` is a readable source, so a
        // qualified `t.c` value in the `SET` binds to the dropped column and breaks
        // the trigger. graphite used to bail on any `UPDATE … FROM` and accept the
        // drop, leaving the trigger silently broken.
        (
            "CREATE TRIGGER tr AFTER INSERT ON s BEGIN UPDATE s SET z = t.c FROM t WHERE s.z = t.a; END",
            "error in trigger tr after drop column: no such column: t.c",
        ),
        // The same reached by a globally-unique bare `c` (only `t` has a `c`).
        (
            "CREATE TRIGGER tr AFTER INSERT ON s BEGIN UPDATE s SET z = c FROM t WHERE s.z = t.a; END",
            "error in trigger tr after drop column: no such column: c",
        ),
    ];
    for (setup, msg) in cases {
        let mut c = Connection::open_memory().unwrap();
        c.execute("CREATE TABLE t(a, b, c)").unwrap();
        c.execute("CREATE TABLE s(z)").unwrap();
        c.execute("CREATE TABLE log(x)").unwrap();
        c.execute(setup).unwrap();
        assert_eq!(
            err(&mut c, "ALTER TABLE t DROP COLUMN c"),
            *msg,
            "for {setup}"
        );
    }
}

#[test]
fn assignment_targets_do_not_block_the_drop() {
    // The dropped column appears only as an assignment *target*, never as a
    // value — sqlite allows the drop (the rewrite simply removes the target).
    for setup in [
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET c = 1; END",
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET c = 1 WHERE a = 1; END",
        "CREATE TRIGGER tr AFTER INSERT ON s BEGIN INSERT INTO t(c) VALUES(1); END",
        "CREATE TRIGGER tr AFTER UPDATE OF c ON t BEGIN INSERT INTO log VALUES(1); END",
    ] {
        let mut c = Connection::open_memory().unwrap();
        c.execute("CREATE TABLE t(a, b, c)").unwrap();
        c.execute("CREATE TABLE s(z)").unwrap();
        c.execute("CREATE TABLE log(x)").unwrap();
        c.execute(setup).unwrap();
        c.execute("ALTER TABLE t DROP COLUMN c")
            .unwrap_or_else(|e| panic!("{setup}: {e}"));
        c.query("SELECT a, b FROM t").unwrap();
    }
}

#[test]
fn a_dependent_not_referencing_the_column_drops_cleanly() {
    for setup in [
        &["CREATE VIEW v AS SELECT a, b FROM t"][..],
        &["CREATE VIEW v AS SELECT * FROM t"][..], // wildcard re-expands; no bare `c`
        &["CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.a); END"][..],
        // A different table whose own view uses a column named `c`.
        &["CREATE TABLE u(c, d)", "CREATE VIEW v AS SELECT c FROM u"][..],
    ] {
        let mut c = Connection::open_memory().unwrap();
        c.execute("CREATE TABLE t(a, b, c)").unwrap();
        c.execute("CREATE TABLE log(x)").unwrap();
        for s in setup {
            c.execute(s).unwrap();
        }
        c.execute("ALTER TABLE t DROP COLUMN c")
            .unwrap_or_else(|e| panic!("{setup:?}: {e}"));
        c.query("SELECT a, b FROM t").unwrap();
    }
}

#[test]
fn the_first_object_in_schema_order_is_reported() {
    // A view created before a trigger is reported first, and vice versa.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b, c)").unwrap();
    c.execute("CREATE TABLE log(x)").unwrap();
    c.execute("CREATE VIEW v AS SELECT c FROM t").unwrap();
    c.execute("CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.c); END")
        .unwrap();
    assert_eq!(
        err(&mut c, "ALTER TABLE t DROP COLUMN c"),
        "error in view v after drop column: no such column: c",
    );

    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b, c)").unwrap();
    c.execute("CREATE TABLE log(x)").unwrap();
    c.execute("CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.c); END")
        .unwrap();
    c.execute("CREATE VIEW v AS SELECT c FROM t").unwrap();
    assert_eq!(
        err(&mut c, "ALTER TABLE t DROP COLUMN c"),
        "error in trigger tr after drop column: no such column: NEW.c",
    );
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
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    let base = "CREATE TABLE t(a,b,c); CREATE TABLE s(z); CREATE TABLE log(x);";
    for sql in [
        // views, value-position refs (reject)
        "CREATE VIEW v AS SELECT c FROM t; ALTER TABLE t DROP COLUMN c;",
        "CREATE VIEW v AS SELECT t.c FROM t; ALTER TABLE t DROP COLUMN c;",
        "CREATE VIEW v AS SELECT x.c FROM t x; ALTER TABLE t DROP COLUMN c;",
        "CREATE VIEW v AS SELECT a FROM t WHERE c>0; ALTER TABLE t DROP COLUMN c;",
        // views that survive
        "CREATE VIEW v AS SELECT a,b FROM t; ALTER TABLE t DROP COLUMN c; SELECT 'ok';",
        "CREATE VIEW v AS SELECT * FROM t; ALTER TABLE t DROP COLUMN c; SELECT 'ok';",
        // triggers, value-position refs (reject)
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.c); END; ALTER TABLE t DROP COLUMN c;",
        "CREATE TRIGGER tr AFTER DELETE ON t BEGIN INSERT INTO log VALUES(OLD.c); END; ALTER TABLE t DROP COLUMN c;",
        "CREATE TRIGGER tr AFTER INSERT ON t WHEN NEW.c>0 BEGIN INSERT INTO log VALUES(1); END; ALTER TABLE t DROP COLUMN c;",
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN DELETE FROM t WHERE c=1; END; ALTER TABLE t DROP COLUMN c;",
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET b=c+1; END; ALTER TABLE t DROP COLUMN c;",
        "CREATE TRIGGER tr AFTER INSERT ON s BEGIN INSERT INTO log SELECT c FROM t; END; ALTER TABLE t DROP COLUMN c;",
        // triggers whose only mention of c is an assignment target (survive)
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET c=1; END; ALTER TABLE t DROP COLUMN c; SELECT 'ok';",
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET c=1 WHERE a=1; END; ALTER TABLE t DROP COLUMN c; SELECT 'ok';",
        "CREATE TRIGGER tr AFTER INSERT ON s BEGIN INSERT INTO t(c) VALUES(1); END; ALTER TABLE t DROP COLUMN c; SELECT 'ok';",
        "CREATE TRIGGER tr AFTER UPDATE OF c ON t BEGIN INSERT INTO log VALUES(1); END; ALTER TABLE t DROP COLUMN c; SELECT 'ok';",
        // triggers reaching c only through a multi-source body statement (reject):
        // an `UPDATE … FROM t`, a `SET (…) = (SELECT … FROM t)` row-assignment,
        // and an `ON CONFLICT … DO UPDATE SET … = (SELECT … FROM t)` upsert.
        "CREATE TRIGGER tr AFTER INSERT ON s BEGIN UPDATE s SET z=t.c FROM t WHERE s.z=t.a; END; ALTER TABLE t DROP COLUMN c;",
        "CREATE TRIGGER tr AFTER INSERT ON s BEGIN UPDATE s SET (z)=(SELECT c FROM t LIMIT 1); END; ALTER TABLE t DROP COLUMN c;",
        "CREATE TABLE p(k PRIMARY KEY,w); CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO p VALUES(1,2) ON CONFLICT(k) DO UPDATE SET w=(SELECT c FROM t LIMIT 1); END; ALTER TABLE t DROP COLUMN c;",
        // …but the same shapes referencing only a *surviving* column drop cleanly.
        "CREATE TRIGGER tr AFTER INSERT ON s BEGIN UPDATE s SET z=t.a FROM t WHERE s.z=t.b; END; ALTER TABLE t DROP COLUMN c; SELECT 'ok';",
        // schema-order precedence
        "CREATE VIEW v AS SELECT c FROM t; CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.c); END; ALTER TABLE t DROP COLUMN c;",
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.c); END; CREATE VIEW v AS SELECT c FROM t; ALTER TABLE t DROP COLUMN c;",
    ] {
        let full = format!("{base}{sql}");
        assert_eq!(run("sqlite3", &full), run(g, &full), "for {sql}");
    }
}
