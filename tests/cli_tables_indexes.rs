//! `.tables` and `.indexes` print names in SQLite's columnar list layout: sorted
//! by byte order, laid out column-major into `80/(maxlen+2)` columns each
//! `maxlen`-wide and left-justified, with a two-space gap. `.tables` covers
//! tables and views (excluding internal `sqlite_*`); `.indexes` covers every
//! index (including auto-created ones). An argument is a `LIKE` pattern.
//! Verified byte-for-byte against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::io::Write;
use std::process::{Command, Stdio};

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn cmd(bin: &str, db: &str, dot: &str) -> String {
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{dot}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn tables_and_indexes_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir();
    let uniq = std::process::id();
    let db = dir.join(format!("gsql_cli_list_{uniq}.db"));
    let db = db.to_str().unwrap();
    let _ = std::fs::remove_file(db);

    // A spread of name lengths (to exercise the column-count math), mixed case
    // (to check byte-order sort: uppercase before lowercase), views, and both a
    // named and an auto-created (UNIQUE) index.
    let mut ddl = String::from("CREATE TABLE Zebra(a UNIQUE, b);CREATE TABLE apple(x);");
    for i in 1..=12 {
        ddl.push_str(&format!("CREATE TABLE table_number_{i}(c);"));
    }
    ddl.push_str("CREATE VIEW v_short AS SELECT 1;CREATE INDEX ix ON apple(x);");
    assert!(
        Command::new("sqlite3")
            .arg(db)
            .arg(&ddl)
            .status()
            .unwrap()
            .success()
    );

    for dot in [".tables", ".indexes", ".tables table%", ".indexes apple"] {
        assert_eq!(
            cmd("sqlite3", db, dot),
            cmd(g, db, dot),
            "mismatch for `{dot}`"
        );
    }
    let _ = std::fs::remove_file(db);
}
