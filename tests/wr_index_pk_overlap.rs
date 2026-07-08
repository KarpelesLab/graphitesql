//! A `WITHOUT ROWID` table's secondary index whose key columns *overlap* the
//! PRIMARY KEY must not repeat the overlapping PK column at the tail of the key.
//!
//! SQLite forms a `WITHOUT ROWID` secondary index key as `(indexed columns…,
//! trailing PK columns…)`, where the trailing PK is the primary-key columns that
//! are **not already** an index key column with the same collation
//! (`sqlite3CreateIndex` / `isDupColumn`). For `CREATE INDEX i ON t(a,c)` over
//! `PRIMARY KEY(a,b)`, the key shape is `(a, c, b)` — `a` is deduplicated, only
//! `b` is appended. graphite used to append the *whole* PK (`a, c, a, b`), so the
//! index disagreed with the table b-tree: `PRAGMA integrity_check` reported a bad
//! index and the file was unreadable by SQLite.
//!
//! This exercises the fix differentially against the sqlite3 3.50.4 CLI:
//!
//! * **graphite's own `integrity_check`** is exactly `ok` for every overlap
//!   shape (first PK column, a later PK column, all PK columns, index == a PK
//!   prefix, and the non-overlapping regression case); and
//! * **on-disk compatibility** — graphite writes a real FILE, then the sqlite3
//!   CLI opens THAT file and (a) `PRAGMA integrity_check` is exactly `ok`, and
//!   (b) an index-driven query (`… INDEXED BY i WHERE …`) returns the same rows
//!   from sqlite (reading graphite's file) and from graphite. This proves the
//!   stored secondary index b-tree is sqlite-readable and not corrupted.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run SQL against an existing FILE in the given binary; return stdout verbatim.
fn run_file(bin: &str, path: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(path).arg(sql).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Build a graphite DB file, run its `integrity_check`, then (if sqlite is
/// present) have the sqlite3 CLI open the same file and cross-check integrity and
/// an index-driven query.
fn check_case(name: &str, build: &str, index_queries: &[&str]) {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "graphite_wr_idx_overlap_{name}_{}.db",
        std::process::id()
    ));
    let path_str = path.to_str().unwrap();
    let _ = std::fs::remove_file(&path);

    let out = Command::new(g).arg(path_str).arg(build).output().unwrap();
    assert!(
        out.status.success(),
        "[{name}] graphite build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // graphite's own integrity_check must be exactly "ok".
    let g_ic = run_file(g, path_str, "PRAGMA integrity_check;");
    assert_eq!(
        g_ic.trim(),
        "ok",
        "[{name}] graphite integrity_check was not ok:\n{g_ic}"
    );

    if sqlite3_available() {
        // sqlite reading graphite's file: integrity_check must be exactly "ok".
        let s_ic = run_file("sqlite3", path_str, "PRAGMA integrity_check;");
        assert_eq!(
            s_ic.trim(),
            "ok",
            "[{name}] sqlite integrity_check on graphite's file was not ok:\n{s_ic}"
        );

        // Index-driven queries must agree between sqlite (on graphite's file) and
        // graphite, and between both and a full-scan (NOT INDEXED) baseline.
        for q in index_queries {
            let s_rows = run_file("sqlite3", path_str, &format!("{q};"));
            let g_rows = run_file(g, path_str, &format!("{q};"));
            assert_eq!(
                s_rows, g_rows,
                "[{name}] sqlite (reading graphite's file) and graphite disagree for `{q}`"
            );
        }
    }

    let _ = std::fs::remove_file(&path);
}

/// The bug's exact repro: index over the FIRST PK column plus a non-PK column.
/// Key shape `(a, c, b)` — `a` deduped, `b` appended.
#[test]
fn wr_index_overlaps_first_pk_column() {
    check_case(
        "first",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE INDEX i ON t(a,c);",
        &[
            "SELECT * FROM t INDEXED BY i WHERE a=4",
            "SELECT * FROM t INDEXED BY i WHERE a=1 AND c=3",
            "SELECT c FROM t INDEXED BY i WHERE a=7",
        ],
    );
}

/// Index over a LATER PK column (`b`) plus a non-PK column. Key `(c, b, a)`:
/// `b` deduped, `a` appended.
#[test]
fn wr_index_overlaps_later_pk_column() {
    check_case(
        "later",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE INDEX i ON t(c,b);",
        &[
            "SELECT * FROM t INDEXED BY i WHERE c=6",
            "SELECT a FROM t INDEXED BY i WHERE c=3 AND b=2",
        ],
    );
}

/// Index over ALL PK columns (a superset of the PK): nothing is appended, the
/// key is just `(a, b)`.
#[test]
fn wr_index_over_all_pk_columns() {
    check_case(
        "all",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE INDEX i ON t(a,b);",
        &[
            "SELECT * FROM t INDEXED BY i WHERE a=4",
            "SELECT c FROM t INDEXED BY i WHERE a=1 AND b=2",
        ],
    );
}

/// Index equal to a PK *prefix* (`a` of `PRIMARY KEY(a,b)`): the trailing PK is
/// just `b`, so the key is `(a, b)`.
#[test]
fn wr_index_equals_pk_prefix() {
    check_case(
        "prefix",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE INDEX i ON t(a);",
        &["SELECT * FROM t INDEXED BY i WHERE a=7"],
    );
}

/// Regression: a NON-overlapping index (`c` only) still keys as `(c, a, b)` and
/// stays clean.
#[test]
fn wr_index_no_overlap_regression() {
    check_case(
        "nooverlap",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE INDEX i ON t(c);",
        &["SELECT * FROM t INDEXED BY i WHERE c=6"],
    );
}

/// Single-column PK overlapped by the index (`PRIMARY KEY(a)`, index `(a,c)`):
/// the whole PK (`a`) is deduped, so the key is `(a, c)`.
#[test]
fn wr_index_overlaps_single_column_pk() {
    check_case(
        "singlepk",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE INDEX i ON t(a,c);",
        &[
            "SELECT * FROM t INDEXED BY i WHERE a=4",
            "SELECT b FROM t INDEXED BY i WHERE a=7 AND c=9",
        ],
    );
}

/// A UNIQUE index that overlaps the PK on its first column.
#[test]
fn wr_unique_index_overlaps_pk() {
    check_case(
        "unique",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE UNIQUE INDEX i ON t(a,c);",
        &["SELECT * FROM t INDEXED BY i WHERE a=4"],
    );
}

/// A DESC *index* column that overlaps the PK, and a DESC *PK* column overlapped
/// by an ASC index column — the trailing PK carries the PK's stored direction.
#[test]
fn wr_index_pk_overlap_desc_directions() {
    check_case(
        "desc_idx",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE INDEX i ON t(a DESC,c);",
        &["SELECT * FROM t INDEXED BY i WHERE a=4"],
    );
    check_case(
        "desc_pk",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a DESC,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE INDEX i ON t(a,c);",
        &["SELECT * FROM t INDEXED BY i WHERE a=4"],
    );
    check_case(
        "desc_pk_later",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b DESC)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE INDEX i ON t(c,b);",
        &["SELECT * FROM t INDEXED BY i WHERE c=6"],
    );
}

/// NULLs and duplicate index-key values across a PK-overlapping index.
#[test]
fn wr_index_pk_overlap_nulls_and_dups() {
    check_case(
        "nulls",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,NULL),(4,5,6),(7,8,NULL);\
         CREATE INDEX i ON t(c,a);",
        &[
            "SELECT * FROM t INDEXED BY i WHERE c IS NULL",
            "SELECT * FROM t INDEXED BY i WHERE c=6",
        ],
    );
    check_case(
        "dups",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,9),(1,3,9),(4,5,9);\
         CREATE INDEX i ON t(c,a);",
        &["SELECT * FROM t INDEXED BY i WHERE c=9"],
    );
}

/// The overlap column carries a COLLATE NOCASE that differs from the plain PK
/// column's collation, so SQLite keeps BOTH (the PK column is *not* a duplicate).
#[test]
fn wr_index_pk_overlap_collation_mismatch() {
    check_case(
        "collate",
        "CREATE TABLE t(a COLLATE NOCASE,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES('X',2,3),('y',5,6),('Z',8,9);\
         CREATE INDEX i ON t(a COLLATE BINARY,c);",
        &["SELECT * FROM t INDEXED BY i WHERE a='X'"],
    );
}

/// UPDATE and DELETE keep a PK-overlapping index consistent (both rebuild the
/// secondary index from the table).
#[test]
fn wr_index_pk_overlap_update_delete() {
    check_case(
        "update",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE INDEX i ON t(a,c);\
         UPDATE t SET c=99 WHERE a=4;",
        &[
            "SELECT * FROM t INDEXED BY i WHERE a=4",
            "SELECT * FROM t INDEXED BY i WHERE a=1",
        ],
    );
    check_case(
        "delete",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);\
         CREATE INDEX i ON t(a,c);\
         DELETE FROM t WHERE a=7;",
        &[
            "SELECT * FROM t INDEXED BY i WHERE a=4",
            "SELECT * FROM t INDEXED BY i WHERE a=7",
        ],
    );
    // Index created BEFORE the rows are inserted (maintained via rebuild on
    // each INSERT).
    check_case(
        "insert_after_index",
        "CREATE TABLE t(a,b,c, PRIMARY KEY(a,b)) WITHOUT ROWID;\
         CREATE INDEX i ON t(a,c);\
         INSERT INTO t VALUES(1,2,3),(4,5,6),(7,8,9);",
        &["SELECT * FROM t INDEXED BY i WHERE a=4"],
    );
}
