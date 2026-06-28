//! The table-valued form of a PRAGMA (`SELECT … FROM pragma_<name>`) is a valid
//! `FROM` source only for a pragma SQLite exposes as an eponymous virtual table.
//! An unrecognized name — a typo, or a statement-only pragma like `wal_checkpoint`
//! / `mmap_size` whose TVF form SQLite rejects — is `no such table: pragma_<name>`,
//! not a silently-empty result (graphite used to return zero rows). A recognized
//! result pragma (`table_info`, `index_list`, getters like `user_version`) keeps
//! working. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn unknown_pragma_tvf_is_no_such_table() {
    let c = Connection::open_memory().unwrap();
    // A pure typo and the statement-only pragmas SQLite does not expose as TVFs.
    for name in [
        "made_up_zzz",
        "wal_checkpoint",
        "wal_autocheckpoint",
        "mmap_size",
        "incremental_vacuum",
        "legacy_file_format",
        "case_sensitive_like",
    ] {
        let sql = format!("SELECT * FROM pragma_{name}");
        let err = c.query(&sql).unwrap_err().to_string();
        assert_eq!(
            err.trim_start_matches("error: "),
            format!("no such table: pragma_{name}"),
            "{sql}"
        );
    }
}

#[test]
fn recognized_pragma_tvf_still_works() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
        .unwrap();
    c.execute("CREATE INDEX i ON t(b)").unwrap();
    // Schema-introspection TVFs.
    assert_eq!(
        c.query("SELECT name FROM pragma_table_info('t') ORDER BY cid")
            .unwrap()
            .rows
            .len(),
        2
    );
    assert_eq!(
        c.query("SELECT name FROM pragma_index_list('t')")
            .unwrap()
            .rows
            .len(),
        1
    );
    // A plain getter pragma is exposed too (one value row).
    assert_eq!(
        c.query("SELECT * FROM pragma_user_version").unwrap().rows,
        vec![vec![graphitesql::Value::Integer(0)]]
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Compare the first non-caret diagnostic line (or stdout), with the engine's
    // wrapper prefixes normalized away — the message body is what must agree.
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
            .trim_start_matches("Parse error near line 1: ")
            .trim_start_matches("Runtime error near line 1: ")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "SELECT * FROM pragma_made_up_zzz;",
        "SELECT * FROM pragma_wal_checkpoint;",
        "SELECT * FROM pragma_mmap_size;",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b); SELECT name FROM pragma_table_info('t') ORDER BY cid;",
        "CREATE TABLE t(a, b); CREATE INDEX i ON t(b); SELECT name FROM pragma_index_list('t');",
        "SELECT * FROM pragma_user_version;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
