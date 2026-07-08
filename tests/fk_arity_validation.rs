//! A `FOREIGN KEY` declaration's child- and parent-column counts must agree at
//! CREATE time, matching SQLite (which validates the arity structurally, before
//! the parent table need even exist):
//!
//!   * a table-level `FOREIGN KEY(a,b) REFERENCES x(y)` whose explicit parent
//!     list has a different length is "number of columns in foreign key does not
//!     match the number of columns in the referenced table";
//!   * a column-level `a REFERENCES x(y,z)` may name only one parent column —
//!     "foreign key on a should reference only one column of table x".
//!
//! An empty parent list (`REFERENCES x`) defers to the parent's PRIMARY KEY and
//! is always accepted here. graphite previously accepted every malformed form
//! silently. Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn fk_column_count_mismatch_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    // table-level: 1 child vs 2 parent columns.
    assert!(
        c.execute("CREATE TABLE t(a, b, FOREIGN KEY(a) REFERENCES x(y,z))")
            .unwrap_err()
            .to_string()
            .contains("number of columns in foreign key does not match")
    );
    // table-level: 2 child vs 1 parent column.
    assert!(
        c.execute("CREATE TABLE t(a, b, FOREIGN KEY(a,b) REFERENCES x(y))")
            .unwrap_err()
            .to_string()
            .contains("number of columns in foreign key does not match")
    );
}

#[test]
fn column_level_fk_references_one_column() {
    let mut c = Connection::open_memory().unwrap();
    assert!(
        c.execute("CREATE TABLE t(a REFERENCES x(y,z))")
            .unwrap_err()
            .to_string()
            .contains("foreign key on a should reference only one column of table x")
    );
}

#[test]
fn well_formed_foreign_keys_are_accepted() {
    let mut c = Connection::open_memory().unwrap();
    // Matching counts, an empty parent list, and a single-column reference all
    // remain valid.
    c.execute("CREATE TABLE t1(a, b, FOREIGN KEY(a,b) REFERENCES x(y,z))")
        .unwrap();
    c.execute("CREATE TABLE t2(a, b, FOREIGN KEY(a) REFERENCES x)")
        .unwrap();
    c.execute("CREATE TABLE t3(a REFERENCES x)").unwrap();
    c.execute("CREATE TABLE t4(a REFERENCES x(y))").unwrap();
    // And a real enforced FK between two existing tables still works.
    c.execute("CREATE TABLE p(id INTEGER PRIMARY KEY)").unwrap();
    c.execute("CREATE TABLE ch(pid REFERENCES p(id))").unwrap();
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
            .next()
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string()
    };
    for sql in [
        "CREATE TABLE t(a, b, FOREIGN KEY(a) REFERENCES x(y,z))",
        "CREATE TABLE t(a, b, FOREIGN KEY(a,b) REFERENCES x(y))",
        "CREATE TABLE t(a, b, FOREIGN KEY(a,b) REFERENCES x(y,z))",
        "CREATE TABLE t(a REFERENCES x(y,z))",
        "CREATE TABLE t(a REFERENCES x(y))",
        "CREATE TABLE t(a REFERENCES x)",
        "CREATE TABLE t(a, b, FOREIGN KEY(a) REFERENCES x)",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
