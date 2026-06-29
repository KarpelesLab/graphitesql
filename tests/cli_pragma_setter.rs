//! The `graphitesql` shell routes a `PRAGMA name = value` setter through
//! `execute` (which can mutate connection state) rather than `query` (`&self`,
//! which would silently no-op). Without this, `PRAGMA foreign_keys=ON` in a
//! one-shot batch had no effect and FK enforcement appeared disabled.

#![cfg(feature = "std")]

use std::process::Command;

fn run(sql: &str) -> (String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_graphitesql"))
        .arg(":memory:")
        .arg(sql)
        .output()
        .expect("run graphitesql shell");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (s, out.status.success())
}

#[test]
fn pragma_setter_persists_in_batch() {
    // Setting then reading back in the same batch reflects the new value.
    let (out, ok) = run("PRAGMA foreign_keys=ON; PRAGMA foreign_keys");
    assert!(ok, "batch should succeed: {out}");
    assert_eq!(out.trim(), "1");

    // The spaced form works too.
    let (out, _) = run("PRAGMA foreign_keys = ON; PRAGMA foreign_keys");
    assert_eq!(out.trim(), "1");

    // A different settable pragma round-trips.
    let (out, _) = run("PRAGMA user_version=42; PRAGMA user_version");
    assert_eq!(out.trim(), "42");
}

#[test]
fn foreign_keys_enforced_through_the_shell() {
    // With FKs enabled in the batch, an orphan child INSERT now fails.
    let (out, ok) = run("PRAGMA foreign_keys=ON; CREATE TABLE u(x PRIMARY KEY); \
         CREATE TABLE t(a REFERENCES u(x)); INSERT INTO t VALUES(99)");
    assert!(!ok, "orphan insert should fail with FKs on: {out}");
    assert!(
        out.contains("FOREIGN KEY constraint failed"),
        "unexpected output: {out}"
    );
}

#[test]
fn getter_pragmas_still_print_rows() {
    // A getter pragma (no '=') still returns its value via query.
    let (out, _) = run("PRAGMA foreign_keys");
    assert_eq!(out.trim(), "0");
    // A function-form getter still lists rows.
    let (out, _) = run("CREATE TABLE t(a INT, b TEXT); PRAGMA table_info(t)");
    assert!(out.contains("a") && out.contains("b"), "got: {out}");
}

#[test]
fn arg_taking_query_pragmas_route_via_eq_form() {
    // A row-returning pragma that takes an argument accepts both `(arg)` and
    // `=arg`. The `=arg` form contains '=' but is a getter, not a setter — it
    // must route to `query` and print its rows, matching `(arg)` and sqlite.
    let setup = "CREATE TABLE foo(a INT PRIMARY KEY, b TEXT REFERENCES bar(id)); \
                 CREATE TABLE bar(id INT PRIMARY KEY); CREATE INDEX ix ON foo(b); ";

    // The `=arg` form yields the same rows as the `(arg)` form.
    for (eq, paren) in [
        ("PRAGMA table_info=foo", "PRAGMA table_info(foo)"),
        ("PRAGMA table_xinfo=foo", "PRAGMA table_xinfo(foo)"),
        ("PRAGMA index_list=foo", "PRAGMA index_list(foo)"),
        ("PRAGMA index_info=ix", "PRAGMA index_info(ix)"),
        ("PRAGMA index_xinfo=ix", "PRAGMA index_xinfo(ix)"),
        (
            "PRAGMA foreign_key_list=foo",
            "PRAGMA foreign_key_list(foo)",
        ),
    ] {
        let (a, _) = run(&format!("{setup}{eq}"));
        let (b, _) = run(&format!("{setup}{paren}"));
        assert_eq!(a, b, "`{eq}` should equal `{paren}`");
        assert!(!a.trim().is_empty(), "`{eq}` should print rows, got empty");
    }

    // Differential: each `=arg` form must match the sqlite3 CLI byte-for-byte.
    if Command::new("sqlite3").arg("--version").output().is_ok() {
        for tail in [
            "PRAGMA table_info=foo",
            "PRAGMA table_xinfo=foo",
            "PRAGMA index_list=foo",
            "PRAGMA index_info=ix",
            "PRAGMA index_xinfo=ix",
            "PRAGMA foreign_key_list=foo",
            "PRAGMA foreign_key_check=foo",
        ] {
            let sql = format!("{setup}{tail}");
            let (g, _) = run(&sql);
            let s = Command::new("sqlite3")
                .arg(":memory:")
                .arg(&sql)
                .output()
                .expect("run sqlite3");
            let s = String::from_utf8_lossy(&s.stdout).into_owned();
            assert_eq!(g.trim_end(), s.trim_end(), "mismatch for `{tail}`");
        }
    }
}

#[test]
fn trigger_bodies_and_returning() {
    // A trigger's BEGIN…END body contains internal ';' — the splitter must keep
    // the whole CREATE TRIGGER together (it previously broke at the first ';').
    let (out, ok) = run(
        "CREATE TABLE t(a); CREATE TABLE c(n); INSERT INTO c VALUES(0); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE c SET n=n+1; END; \
         INSERT INTO t VALUES(1),(2),(3); SELECT n FROM c",
    );
    assert!(ok, "trigger script should run: {out}");
    assert_eq!(out.trim(), "3");

    // A transaction BEGIN/END (not a trigger body) still splits normally.
    let (out, ok) =
        run("BEGIN; CREATE TABLE t(x); INSERT INTO t VALUES(1); COMMIT; SELECT x FROM t");
    assert!(ok, "transaction script should run: {out}");
    assert_eq!(out.trim(), "1");

    // INSERT … RETURNING prints the projected rows (run via execute_returning).
    let (out, ok) = run("CREATE TABLE t(a INTEGER PRIMARY KEY, b); \
                         INSERT INTO t(b) VALUES('x') RETURNING a,b");
    assert!(ok, "RETURNING should print rows: {out}");
    assert_eq!(out.trim(), "1|x");
}

#[test]
fn explain_and_with_dml_route_correctly() {
    // EXPLAIN returns rows: it must run via query (returns_rows now includes it),
    // not execute (which rejects EXPLAIN). The plan detail is shown, not an error.
    let (out, ok) = run("CREATE TABLE t(a,b); CREATE INDEX i ON t(a); \
                         EXPLAIN QUERY PLAN SELECT * FROM t WHERE a=1");
    assert!(ok, "EXPLAIN should succeed: {out}");
    // Rendered as SQLite's QUERY PLAN tree, not the raw (id|parent|...) rows.
    assert_eq!(out.trim(), "QUERY PLAN\n`--SEARCH t USING INDEX i (a=?)");

    // A plain table scan.
    let (out, _) = run("CREATE TABLE t(a,b); EXPLAIN QUERY PLAN SELECT * FROM t");
    assert_eq!(out.trim(), "QUERY PLAN\n`--SCAN t");

    // A WITH-prefixed statement that is actually DML looks row-returning (first
    // word WITH) but must fall back to execute.
    let (out, ok) = run(
        "CREATE TABLE t(a); WITH s(v) AS (VALUES(1),(2),(3)) INSERT INTO t SELECT v FROM s; \
         SELECT group_concat(a) FROM (SELECT a FROM t ORDER BY a)",
    );
    assert!(ok, "WITH+INSERT should succeed: {out}");
    assert_eq!(out.trim(), "1,2,3");
}
