//! An unrecognized `PRAGMA` name is silently ignored, matching sqlite: "If the
//! pragma name is not recognized ... no error is raised, the pragma is simply
//! ignored." graphite's write path already no-oped unknown setters
//! (`PRAGMA made_up = 1`); previously the *read* form (`PRAGMA made_up`,
//! `PRAGMA made_up(1)`) errored with "not yet implemented: this PRAGMA". Now
//! both return an empty result. Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn unknown_pragma_returns_no_rows() {
    let c = Connection::open_memory().unwrap();
    for sql in [
        "PRAGMA totally_made_up_pragma",
        "PRAGMA totally_made_up_pragma(7)",
        "PRAGMA totally_made_up_pragma = 1",
        "PRAGMA nope",
    ] {
        let r = c.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(r.rows.is_empty(), "for {sql}: expected no rows, got {r:?}");
    }

    // A recognized pragma still works.
    assert!(!c.query("PRAGMA page_size").unwrap().rows.is_empty());
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let run = |bin: &str, sql: &str| -> (String, String) {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        (
            String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
            String::from_utf8_lossy(&out.stderr).trim_end().to_string(),
        )
    };
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        "PRAGMA totally_made_up_pragma",
        "PRAGMA totally_made_up_pragma(7)",
        "PRAGMA totally_made_up_pragma = 1",
        "PRAGMA nope",
    ] {
        let (s_out, s_err) = run("sqlite3", sql);
        let (g_out, g_err) = run(g, sql);
        assert_eq!(s_out, g_out, "stdout for {sql}");
        assert_eq!(s_err, g_err, "stderr for {sql}");
    }
}
