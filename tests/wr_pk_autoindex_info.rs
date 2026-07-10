//! `PRAGMA index_info` / `index_xinfo` for a `WITHOUT ROWID` table's implicit
//! PRIMARY KEY index (`sqlite_autoindex_<t>_1`).
//!
//! A WITHOUT ROWID table is stored as a b-tree clustered on its PRIMARY KEY, so the
//! PK "index" has no separate schema object — but SQLite still reports it as
//! `sqlite_autoindex_<t>_1` and answers `index_info`/`index_xinfo` for it: the key
//! columns are the PK columns (in key order, honouring each `DESC` and collation),
//! and `index_xinfo` additionally lists the remaining table columns as trailing
//! auxiliary (non-key) columns. graphite kept no index object for it and so
//! returned an empty result; it now synthesizes the same rows.
//!
//! Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s.trim_end().to_string()
}

#[test]
fn without_rowid_pk_autoindex_info_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Single-column PK, descending.
        "CREATE TABLE t(a,b,PRIMARY KEY(a DESC)) WITHOUT ROWID; PRAGMA index_info('sqlite_autoindex_t_1')",
        "CREATE TABLE t(a,b,PRIMARY KEY(a DESC)) WITHOUT ROWID; PRAGMA index_xinfo('sqlite_autoindex_t_1')",
        // Composite PK.
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID; PRAGMA index_info('sqlite_autoindex_t_1')",
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a,b)) WITHOUT ROWID; PRAGMA index_xinfo('sqlite_autoindex_t_1')",
        // PK column carrying a collation, and DESC + collation together.
        "CREATE TABLE t(a COLLATE NOCASE, b, PRIMARY KEY(a)) WITHOUT ROWID; PRAGMA index_xinfo('sqlite_autoindex_t_1')",
        "CREATE TABLE t(a COLLATE RTRIM, b, PRIMARY KEY(a DESC)) WITHOUT ROWID; PRAGMA index_xinfo('sqlite_autoindex_t_1')",
        // Table name containing underscores (the auto-index name parses correctly).
        "CREATE TABLE my_tbl(a,b,PRIMARY KEY(a)) WITHOUT ROWID; PRAGMA index_info('sqlite_autoindex_my_tbl_1')",
        // The TVF form.
        "CREATE TABLE t(a,b,PRIMARY KEY(a)) WITHOUT ROWID; SELECT * FROM pragma_index_info('sqlite_autoindex_t_1')",
        // A WITHOUT ROWID table with a secondary index: both resolve.
        "CREATE TABLE t(a,b,c,PRIMARY KEY(a)) WITHOUT ROWID; CREATE INDEX i ON t(b); \
         PRAGMA index_xinfo(i); PRAGMA index_info('sqlite_autoindex_t_1')",
        // A rowid table's UNIQUE auto-index (the pre-existing path) is unchanged.
        "CREATE TABLE t(a UNIQUE, b); PRAGMA index_info('sqlite_autoindex_t_1'); \
         PRAGMA index_xinfo('sqlite_autoindex_t_1')",
        // An unknown auto-index name is an empty result, not an error.
        "CREATE TABLE t(a); PRAGMA index_info('sqlite_autoindex_nope_1')",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
