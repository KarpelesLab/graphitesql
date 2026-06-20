//! Uniqueness enforcement for standalone `CREATE UNIQUE INDEX` — plain,
//! partial, expression, and multi-column — which the inline-constraint path
//! (`TableMeta::unique`) does not cover. Each behaviour is matched against the
//! real `sqlite3` CLI, and every database we write is gated on
//! `PRAGMA integrity_check`.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run a `;`-separated script through `sqlite3` and report whether it succeeded
/// (exit 0) — i.e. no constraint violation aborted it.
fn sqlite3_ok(sql: &str) -> bool {
    Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap()
        .status
        .success()
}

/// Run a `;`-separated script through graphitesql, returning whether every
/// statement succeeded.
fn graphite_ok(script: &[&str]) -> bool {
    let mut c = Connection::open_memory().unwrap();
    for s in script {
        if c.execute(s).is_err() {
            return false;
        }
    }
    true
}

/// Assert graphite and sqlite agree on whether `script` runs without a
/// constraint error.
fn agree(script: &[&str]) {
    let g = graphite_ok(script);
    if sqlite3_available() {
        let s = sqlite3_ok(&script.join(";"));
        assert_eq!(g, s, "graphite/sqlite disagree on: {script:?}");
    }
}

#[test]
fn standalone_unique_index_enforced() {
    // Plain single-column unique index.
    agree(&[
        "CREATE TABLE t(a INT)",
        "CREATE UNIQUE INDEX ux ON t(a)",
        "INSERT INTO t VALUES(1)",
        "INSERT INTO t VALUES(1)", // conflict
    ]);
    agree(&[
        "CREATE TABLE t(a INT)",
        "CREATE UNIQUE INDEX ux ON t(a)",
        "INSERT INTO t VALUES(1)",
        "INSERT INTO t VALUES(2)", // distinct, ok
    ]);
    // NULLs are distinct: two NULL keys do not collide.
    agree(&[
        "CREATE TABLE t(a INT)",
        "CREATE UNIQUE INDEX ux ON t(a)",
        "INSERT INTO t VALUES(NULL)",
        "INSERT INTO t VALUES(NULL)",
    ]);
}

#[test]
fn unique_index_collation_and_expression() {
    // Expression index: lower(x) collides for 'Hello'/'HELLO'.
    agree(&[
        "CREATE TABLE t(x TEXT)",
        "CREATE UNIQUE INDEX ux ON t(lower(x))",
        "INSERT INTO t VALUES('Hello')",
        "INSERT INTO t VALUES('HELLO')",
    ]);
    // NOCASE collation on the index column.
    agree(&[
        "CREATE TABLE t(x TEXT)",
        "CREATE UNIQUE INDEX ux ON t(x COLLATE NOCASE)",
        "INSERT INTO t VALUES('abc')",
        "INSERT INTO t VALUES('ABC')",
    ]);
}

#[test]
fn unique_index_partial_and_multicolumn() {
    // Partial unique index: collision only among rows passing the predicate.
    agree(&[
        "CREATE TABLE t(a INT, b INT)",
        "CREATE UNIQUE INDEX ux ON t(a) WHERE b > 0",
        "INSERT INTO t VALUES(1, 1)",
        "INSERT INTO t VALUES(1, 5)", // both pass predicate -> conflict
    ]);
    agree(&[
        "CREATE TABLE t(a INT, b INT)",
        "CREATE UNIQUE INDEX ux ON t(a) WHERE b > 0",
        "INSERT INTO t VALUES(1, 1)",
        "INSERT INTO t VALUES(1, -5)", // second excluded -> ok
    ]);
    // Multi-column: a NULL in any key term makes the row distinct.
    agree(&[
        "CREATE TABLE t(a INT, b INT)",
        "CREATE UNIQUE INDEX ux ON t(a, b)",
        "INSERT INTO t VALUES(1, 2)",
        "INSERT INTO t VALUES(1, 2)", // conflict
    ]);
    agree(&[
        "CREATE TABLE t(a INT, b INT)",
        "CREATE UNIQUE INDEX ux ON t(a, b)",
        "INSERT INTO t VALUES(1, NULL)",
        "INSERT INTO t VALUES(1, NULL)", // distinct (NULL)
    ]);
}

#[test]
fn unique_index_update_conflict() {
    agree(&[
        "CREATE TABLE t(a INT)",
        "CREATE UNIQUE INDEX ux ON t(a)",
        "INSERT INTO t VALUES(1)",
        "INSERT INTO t VALUES(2)",
        "UPDATE t SET a = 1 WHERE a = 2", // collides with existing 1
    ]);
}

#[test]
fn unique_index_without_rowid() {
    agree(&[
        "CREATE TABLE t(k INT PRIMARY KEY, a INT) WITHOUT ROWID",
        "CREATE UNIQUE INDEX ux ON t(a)",
        "INSERT INTO t VALUES(1, 10)",
        "INSERT INTO t VALUES(2, 10)", // duplicate a -> conflict
    ]);
    agree(&[
        "CREATE TABLE t(k INT PRIMARY KEY, a INT) WITHOUT ROWID",
        "CREATE UNIQUE INDEX ux ON t(a)",
        "INSERT INTO t VALUES(1, 10)",
        "INSERT INTO t VALUES(2, 20)",     // ok
        "UPDATE t SET a = 10 WHERE k = 2", // update into conflict
    ]);
}

#[test]
fn or_ignore_and_replace_with_unique_index() {
    // OR IGNORE keeps the original row; OR REPLACE swaps it. Verified on a real
    // file so `sqlite3`'s integrity_check gates the resulting index b-tree.
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-uqidx-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    {
        let mut c = Connection::create(&path).unwrap();
        for s in [
            "CREATE TABLE t(a INT, v TEXT)",
            "CREATE UNIQUE INDEX ux ON t(a)",
            "INSERT INTO t VALUES(1, 'x')",
            "INSERT OR IGNORE INTO t VALUES(1, 'y')", // ignored
            "INSERT OR REPLACE INTO t VALUES(2, 'p')",
            "INSERT OR REPLACE INTO t VALUES(2, 'q')", // replaces 'p'
        ] {
            c.execute(s).unwrap();
        }
        let r = c.query("SELECT a, v FROM t ORDER BY a").unwrap();
        assert_eq!(r.rows.len(), 2);
    }

    let check = Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check;")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&check.stdout).trim(),
        "ok",
        "integrity_check failed"
    );
    // sqlite reads our rows: original 1 kept, 2 replaced to 'q'.
    let rows = Command::new("sqlite3")
        .arg(&path)
        .arg("SELECT a||':'||v FROM t ORDER BY a;")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&rows.stdout).trim(), "1:x\n2:q");
    let _ = std::fs::remove_file(&path);
}
