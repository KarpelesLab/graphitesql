//! `PRAGMA automatic_index` and `PRAGMA cell_size_check` are boolean toggles
//! that graphite implements as inert flags — it builds no transient automatic
//! indexes and validates btree cells on every read regardless — but, like
//! sqlite, the stored value must round-trip: setting it and reading it back
//! returns what was set. graphite previously hard-coded the get form to the
//! default (1 for automatic_index, 0 for cell_size_check) and dropped any
//! assignment. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn get(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}

#[test]
fn automatic_index_round_trips() {
    let mut c = Connection::open_memory().unwrap();
    // Default is on, matching sqlite.
    assert_eq!(get(&c, "PRAGMA automatic_index"), Value::Integer(1));
    c.execute("PRAGMA automatic_index = 0").unwrap();
    assert_eq!(get(&c, "PRAGMA automatic_index"), Value::Integer(0));
    c.execute("PRAGMA automatic_index = ON").unwrap();
    assert_eq!(get(&c, "PRAGMA automatic_index"), Value::Integer(1));
}

#[test]
fn cell_size_check_round_trips() {
    let mut c = Connection::open_memory().unwrap();
    // Default is off, matching sqlite.
    assert_eq!(get(&c, "PRAGMA cell_size_check"), Value::Integer(0));
    c.execute("PRAGMA cell_size_check = 1").unwrap();
    assert_eq!(get(&c, "PRAGMA cell_size_check"), Value::Integer(1));
    c.execute("PRAGMA cell_size_check = OFF").unwrap();
    assert_eq!(get(&c, "PRAGMA cell_size_check"), Value::Integer(0));
}

#[test]
fn matches_sqlite_cli() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    };
    for sql in [
        "PRAGMA automatic_index;",
        "PRAGMA automatic_index=0; PRAGMA automatic_index;",
        "PRAGMA automatic_index=0; PRAGMA automatic_index=1; PRAGMA automatic_index;",
        "PRAGMA cell_size_check;",
        "PRAGMA cell_size_check=1; PRAGMA cell_size_check;",
        "PRAGMA cell_size_check=1; PRAGMA cell_size_check=0; PRAGMA cell_size_check;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
