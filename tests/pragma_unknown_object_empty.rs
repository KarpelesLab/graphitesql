//! Schema-introspection PRAGMAs report an *unknown* object as an empty result,
//! not an error — matching SQLite. `index_info`/`index_xinfo` on a nonexistent
//! index, and `foreign_key_list` on a nonexistent table, each return zero rows
//! with the usual column headers. graphite previously raised `no such index: …`
//! / `no such table: …`. A real object is unaffected. Verified against the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn unknown_index_and_table_yield_empty_results() {
    let c = Connection::open_memory().unwrap();
    for pragma in [
        "PRAGMA index_info(nope)",
        "PRAGMA index_xinfo(nope)",
        "PRAGMA foreign_key_list(nope)",
    ] {
        let r = c.query(pragma).unwrap();
        assert!(r.rows.is_empty(), "{pragma} should be empty");
        assert!(!r.columns.is_empty(), "{pragma} keeps its headers");
    }
    // The headers are the standard ones.
    assert_eq!(
        c.query("PRAGMA index_info(nope)").unwrap().columns,
        ["seqno", "cid", "name"]
    );
    assert_eq!(
        c.query("PRAGMA index_xinfo(nope)").unwrap().columns,
        ["seqno", "cid", "name", "desc", "coll", "key"]
    );
    assert_eq!(
        c.query("PRAGMA foreign_key_list(nope)").unwrap().columns,
        [
            "id",
            "seq",
            "table",
            "from",
            "to",
            "on_update",
            "on_delete",
            "match"
        ]
    );
}

#[test]
fn a_real_object_still_reports_its_rows() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE INDEX i ON t(a, b)").unwrap();
    assert_eq!(c.query("PRAGMA index_info(i)").unwrap().rows.len(), 2);
    c.execute("CREATE TABLE c(x, y, FOREIGN KEY(x) REFERENCES t(a))")
        .unwrap();
    assert_eq!(c.query("PRAGMA foreign_key_list(c)").unwrap().rows.len(), 1);
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
    for sql in [
        "PRAGMA index_info(nope);",
        "PRAGMA index_xinfo(nope);",
        "CREATE TABLE t(a); PRAGMA index_info(nope);",
        "PRAGMA foreign_key_list(nope);",
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(a,b); PRAGMA index_info(i);",
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(a); PRAGMA index_xinfo(i);",
        "CREATE TABLE t(a,b,UNIQUE(a)); PRAGMA index_info(sqlite_autoindex_t_1);",
        "CREATE TABLE c(x, y, FOREIGN KEY(x) REFERENCES p(id)); PRAGMA foreign_key_list(c);",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
