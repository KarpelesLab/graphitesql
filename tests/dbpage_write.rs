//! Writable `sqlite_dbpage` (dbpage-2): `UPDATE sqlite_dbpage SET data = <blob>
//! WHERE pgno = …` overwrites a page's raw bytes; a `DELETE` is rejected. This is
//! SQLite's `dbpageUpdate` (minus the CLI's defensive-mode gate, which graphite
//! has no equivalent for — a raw `sqlite3_open` connection is likewise writable).
//! Verified against a `sqlite3` 3.50.4 built with `SQLITE_ENABLE_DBPAGE_VTAB` and
//! run with `.dbconfig defensive off`.

#![cfg(feature = "std")]

use std::path::Path;
use std::process::Command;

fn graphite() -> &'static str {
    env!("CARGO_BIN_EXE_graphitesql")
}

fn g(db: &Path, sql: &str) -> (String, String) {
    let o = Command::new(graphite()).arg(db).arg(sql).output().unwrap();
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[test]
fn update_sqlite_dbpage_writes_raw_page_bytes() {
    let dir = std::env::temp_dir().join(format!("gsql_dbpage_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("d.db");
    let _ = std::fs::remove_file(&db);

    g(
        &db,
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,'hello'),(2,'world');\
         CREATE TABLE u(x); INSERT INTO u VALUES(10),(20),(30);",
    );

    // A self-write (data = data) is a byte-preserving no-op: integrity stays ok
    // and the table contents are untouched.
    let (_, e) = g(&db, "UPDATE sqlite_dbpage SET data = data;");
    assert!(e.is_empty(), "self-write should succeed, got `{e}`");
    let (chk, _) = g(&db, "PRAGMA integrity_check;");
    assert_eq!(chk.trim(), "ok");
    let (rows, _) = g(
        &db,
        "SELECT a,b FROM t ORDER BY a; SELECT x FROM u ORDER BY x;",
    );
    assert_eq!(rows, "1|hello\n2|world\n10\n20\n30\n");

    // Copy one page's bytes onto another (both engines produce the same result).
    // Read page 2, write it back — still a valid image of page 2.
    let (_, e) = g(
        &db,
        "UPDATE sqlite_dbpage SET data = (SELECT data FROM sqlite_dbpage WHERE pgno = 2) \
         WHERE pgno = 2;",
    );
    assert!(e.is_empty(), "page round-trip should succeed, got `{e}`");
    assert_eq!(g(&db, "PRAGMA integrity_check;").0.trim(), "ok");

    // Error parity with SQLite's xUpdate.
    let cases = [
        ("DELETE FROM sqlite_dbpage WHERE pgno = 2;", "cannot delete"),
        (
            "UPDATE sqlite_dbpage SET pgno = 5 WHERE pgno = 2;",
            "cannot insert",
        ),
        (
            "UPDATE sqlite_dbpage SET data = x'00' WHERE pgno = 2;",
            "bad page value",
        ),
        (
            "UPDATE sqlite_dbpage SET data = 42 WHERE pgno = 2;",
            "bad page value",
        ),
    ];
    for (sql, want) in cases {
        let (out, err) = g(&db, sql);
        assert!(out.is_empty(), "`{sql}` should not produce rows");
        assert!(
            err.contains(want),
            "`{sql}` should error `{want}`, got `{err}`"
        );
    }

    // A zeroed b-tree page corrupts the database — exactly as in SQLite (the write
    // itself succeeds; the corruption shows up on the next read).
    let (_, e) = g(
        &db,
        "UPDATE sqlite_dbpage SET data = zeroblob(4096) WHERE pgno = 2;",
    );
    assert!(e.is_empty(), "the raw write itself succeeds");
    assert_ne!(
        g(&db, "PRAGMA integrity_check;").0.trim(),
        "ok",
        "a zeroed b-tree page should fail integrity_check"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
