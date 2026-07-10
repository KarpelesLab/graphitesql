//! Advisory connection-setting PRAGMAs round-trip their value like SQLite, even
//! though graphite does not implement the underlying behaviour (it keeps every page
//! resident, is single-threaded, and has no fsync-policy knob). `synchronous`,
//! `temp_store`, and `threads` previously reported a hardcoded default; they now
//! store and report back the set value. `busy_timeout` and `threads` also echo the
//! value on the `= N` set form (as SQLite prints it), while `synchronous` /
//! `temp_store` sets are silent.
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
fn advisory_setting_pragmas_round_trip() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // synchronous: keyword and numeric forms, silent set, echoed query.
        "PRAGMA synchronous=NORMAL; PRAGMA synchronous",
        "PRAGMA synchronous=OFF; PRAGMA synchronous",
        "PRAGMA synchronous=EXTRA; PRAGMA synchronous",
        "PRAGMA synchronous=2; PRAGMA synchronous",
        "PRAGMA synchronous",
        "PRAGMA synchronous=NORMAL", // silent set
        // temp_store.
        "PRAGMA temp_store=MEMORY; PRAGMA temp_store",
        "PRAGMA temp_store=FILE; PRAGMA temp_store",
        "PRAGMA temp_store=2; PRAGMA temp_store",
        "PRAGMA temp_store",
        "PRAGMA temp_store=MEMORY", // silent set
        // threads: the set form echoes the value.
        "PRAGMA threads=4",
        "PRAGMA threads=4; PRAGMA threads",
        "PRAGMA threads=-5; PRAGMA threads",
        "PRAGMA threads",
        // busy_timeout: also echoes on set, unchanged for the getter.
        "PRAGMA busy_timeout=5000",
        "PRAGMA busy_timeout=5000; PRAGMA busy_timeout",
        "PRAGMA busy_timeout",
        // secure_delete (0/1/2=fast) — echoes on set, and its behaviour is preserved.
        "PRAGMA secure_delete=1",
        "PRAGMA secure_delete=1; PRAGMA secure_delete",
        "PRAGMA secure_delete=FAST; PRAGMA secure_delete",
        "PRAGMA secure_delete=0; PRAGMA secure_delete",
        // soft_heap_limit and wal_autocheckpoint — advisory, echo on set.
        "PRAGMA soft_heap_limit=1000",
        "PRAGMA soft_heap_limit=1000; PRAGMA soft_heap_limit",
        "PRAGMA soft_heap_limit=-5; PRAGMA soft_heap_limit",
        "PRAGMA soft_heap_limit",
        "PRAGMA wal_autocheckpoint=100",
        "PRAGMA wal_autocheckpoint=100; PRAGMA wal_autocheckpoint",
        "PRAGMA wal_autocheckpoint",
        // journal_size_limit and analysis_limit — already stored, now echo on set.
        "PRAGMA journal_size_limit=1024",
        "PRAGMA journal_size_limit=1024; PRAGMA journal_size_limit",
        "PRAGMA journal_size_limit=-1; PRAGMA journal_size_limit",
        "PRAGMA analysis_limit=100",
        "PRAGMA analysis_limit=100; PRAGMA analysis_limit",
        "PRAGMA analysis_limit=-5; PRAGMA analysis_limit",
        "PRAGMA analysis_limit",
        // Setting synchronous never affects query results.
        "CREATE TABLE t(a); PRAGMA synchronous=OFF; INSERT INTO t VALUES(1),(2); SELECT * FROM t",
        // Unrelated setters are unaffected.
        "PRAGMA cache_size=-3000; PRAGMA cache_size",
        "PRAGMA journal_mode=MEMORY",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
