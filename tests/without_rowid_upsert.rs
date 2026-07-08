//! UPSERT (`ON CONFLICT … DO NOTHING/UPDATE`) and `RETURNING` on WITHOUT ROWID
//! tables. graphite previously rejected both with a "not yet implemented" error;
//! the clustered-index write path now resolves the conflict against the scanned
//! PRIMARY KEY / UNIQUE rows and edits them in place. Verified byte-for-byte
//! against the sqlite3 3.50.4 CLI.
//!
//! NOTE: a *bare* `ON CONFLICT` (no target) on a row that violates more than one
//! uniqueness constraint is intentionally omitted — sqlite documents that case as
//! undefined ("it is undefined which constraint triggers the upsert").

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn without_rowid_upsert_and_returning() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // DO UPDATE on a composite-PK collision, referencing the old value.
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3);\
         INSERT INTO t VALUES(1,2,9) ON CONFLICT(a,b) DO UPDATE SET c=c+100;\
         SELECT quote(a),quote(b),quote(c) FROM t;",
        // DO NOTHING absorbs the conflicting row.
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3);\
         INSERT INTO t VALUES(1,2,9) ON CONFLICT(a,b) DO NOTHING;\
         SELECT quote(a),quote(b),quote(c) FROM t;",
        // Bare ON CONFLICT + excluded.* on a single-constraint table.
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3);\
         INSERT INTO t VALUES(1,2,9) ON CONFLICT DO UPDATE SET c=excluded.c;\
         SELECT quote(a),quote(b),quote(c) FROM t;",
        // DO UPDATE … WHERE vetoes the update (row still absorbed, no insert).
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3);\
         INSERT INTO t VALUES(1,2,9) ON CONFLICT(a,b) DO UPDATE SET c=excluded.c WHERE c<0;\
         SELECT quote(a),quote(b),quote(c) FROM t;",
        // No conflict — the upsert clause is dormant, the row is inserted.
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3);\
         INSERT INTO t VALUES(5,6,7) ON CONFLICT(a,b) DO UPDATE SET c=99;\
         SELECT quote(a),quote(b),quote(c) FROM t ORDER BY a;",
        // RETURNING on a DO UPDATE.
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3);\
         INSERT INTO t VALUES(1,2,9) ON CONFLICT(a,b) DO UPDATE SET c=100 RETURNING a,b,c;",
        // RETURNING on a plain WITHOUT ROWID insert.
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3) RETURNING c,b,a;",
        // Targeting a secondary UNIQUE constraint (not the PK).
        "CREATE TABLE t(a,b,c UNIQUE,PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6);\
         INSERT INTO t VALUES(1,9,6) ON CONFLICT(c) DO UPDATE SET b=99;\
         SELECT quote(a),quote(b),quote(c) FROM t ORDER BY a;",
        // A DO UPDATE that would duplicate another row's UNIQUE value is an error.
        "CREATE TABLE t(a,b,c UNIQUE,PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6);\
         INSERT INTO t VALUES(1,2,9) ON CONFLICT(a,b) DO UPDATE SET c=6;\
         SELECT quote(a),quote(b),quote(c) FROM t ORDER BY a;",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for `{sql}`");
    }
}

#[test]
fn without_rowid_delete_update_returning() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // DELETE … RETURNING * on a WITHOUT ROWID table.
        "CREATE TABLE t(a,b,PRIMARY KEY(a)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2),(3,4);DELETE FROM t WHERE a=1 RETURNING *;",
        // DELETE-all … RETURNING a column.
        "CREATE TABLE t(a,b,PRIMARY KEY(a)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2),(3,4);DELETE FROM t RETURNING a;",
        // UPDATE … RETURNING computed columns.
        "CREATE TABLE t(a,b,PRIMARY KEY(a)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2),(3,4);UPDATE t SET b=b+10 WHERE a=1 RETURNING a,b;",
        // UPDATE-all … RETURNING an expression.
        "CREATE TABLE t(a,b,PRIMARY KEY(a)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2),(3,4);UPDATE t SET b=b*2 RETURNING b+1;",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for `{sql}`");
    }
}
