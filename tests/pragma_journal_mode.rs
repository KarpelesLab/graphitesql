//! `PRAGMA journal_mode = <mode>` is a setter that *also* reports a result row:
//! SQLite echoes the resulting journal mode (`wal` after a successful switch, or
//! `memory` for an in-memory database that cannot change it, or the unchanged
//! current mode when the requested one is invalid). graphite's CLI routed every
//! `PRAGMA … = …` setter through `execute`, which discards rows, so the setter
//! form printed nothing. It now reads the resulting mode back and prints it.
//! Silent setters (`foreign_keys=ON`, `user_version=5`) stay silent.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, db: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(db).arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s.trim_end().to_string()
}

#[test]
fn journal_mode_setter_echoes_result_in_memory() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        "PRAGMA journal_mode=bogus",
        "PRAGMA journal_mode=wal",
        "PRAGMA journal_mode=delete",
        "PRAGMA journal_mode=memory",
        "PRAGMA journal_mode=off",
        "PRAGMA journal_mode",
        "PRAGMA main.journal_mode=wal",
        // These setters are silent in SQLite — no result row.
        "PRAGMA foreign_keys=ON",
        "PRAGMA user_version=5",
    ] {
        assert_eq!(
            out("sqlite3", ":memory:", sql),
            out(g, ":memory:", sql),
            "for {sql}"
        );
    }
}

#[test]
fn journal_mode_setter_echoes_result_on_file() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir();
    // Each statement opens a fresh database file so the starting mode is the
    // file default (`delete`) every time.
    for (i, mode) in ["bogus", "wal", "delete"].iter().enumerate() {
        let sql = format!("PRAGMA journal_mode={mode}");
        let fs = dir.join(format!("gsql_jm_s_{i}_{}.db", std::process::id()));
        let fg = dir.join(format!("gsql_jm_g_{i}_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&fs);
        let _ = std::fs::remove_file(&fg);
        let a = out("sqlite3", fs.to_str().unwrap(), &sql);
        let b = out(g, fg.to_str().unwrap(), &sql);
        // Clean up WAL/journal sidecars too.
        for p in [&fs, &fg] {
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_file(p.with_extension("db-wal"));
            let _ = std::fs::remove_file(p.with_extension("db-shm"));
        }
        assert_eq!(a, b, "for {sql}");
    }
}
