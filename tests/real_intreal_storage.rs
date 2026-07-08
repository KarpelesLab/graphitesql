//! SQLite stores a whole-number real in a REAL-affinity column with the compact
//! integer serial type (`MEM_IntReal`) rather than an 8-byte float, and reads it
//! back as REAL. graphite wrote an 8-byte float, so its files were larger and
//! did not byte-match sqlite. The value must still read back as REAL. Verified
//! against the sqlite3 3.50.4 CLI, including that sqlite reads graphite's file.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, db: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(db).arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Read a file, zeroing the header fields that legitimately differ between
/// writers: the file change counter (24..28) and the version-valid-for /
/// SQLITE_VERSION_NUMBER fields (92..100).
fn normalized(path: &std::path::Path) -> Vec<u8> {
    let mut b = std::fs::read(path).unwrap();
    for i in (24..28).chain(92..100) {
        if i < b.len() {
            b[i] = 0;
        }
    }
    b
}

#[test]
fn whole_real_stores_as_int_serial_and_byte_matches() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir().join(format!("gsql_intreal_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sq = dir.join("s.db");
    let gr = dir.join("g.db");

    // A plain rowid table with a REAL column: whole numbers store as int serials,
    // the fractional one as a float — and the two files must byte-match (once the
    // volatile header fields are normalized).
    let ddl = "CREATE TABLE t(a REAL, b);\
               INSERT INTO t VALUES(5,'x'),(1000000,'y'),(-3,'z'),(2.5,'q'),(0,'r');";
    let _ = std::fs::remove_file(&sq);
    let _ = std::fs::remove_file(&gr);
    run("sqlite3", sq.to_str().unwrap(), ddl);
    run(g, gr.to_str().unwrap(), ddl);

    // Both engines read the same values (all REAL).
    let q = "SELECT typeof(a)||' '||quote(a) FROM t ORDER BY rowid";
    assert_eq!(
        run("sqlite3", sq.to_str().unwrap(), q),
        run(g, gr.to_str().unwrap(), q),
    );
    // sqlite reads graphite's file identically, and it is structurally sound.
    assert_eq!(
        run("sqlite3", sq.to_str().unwrap(), q),
        run("sqlite3", gr.to_str().unwrap(), q)
    );
    assert_eq!(
        run("sqlite3", gr.to_str().unwrap(), "PRAGMA integrity_check").trim(),
        "ok"
    );
    // The database bytes match sqlite's (record encoding is identical).
    assert_eq!(
        normalized(&sq),
        normalized(&gr),
        "graphite's file should byte-match sqlite's for a REAL column"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
