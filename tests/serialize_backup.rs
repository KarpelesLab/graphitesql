//! `Connection::serialize()` and the shell's `.backup`/`.save` produce a
//! complete, valid SQLite database file. Verified by opening the serialized
//! image with the `sqlite3` CLI (`PRAGMA integrity_check` must be `ok` and the
//! data must round-trip) and by re-opening it in graphite.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn tmp_path(tag: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphite-ser-{}-{tag}.db", std::process::id()));
    p.to_string_lossy().into_owned()
}

/// Run one SQL statement through the sqlite3 CLI against `db`, returning stdout.
fn sqlite3(db: &str, sql: &str) -> String {
    let out = Command::new("sqlite3").arg(db).arg(sql).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const SETUP: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c REAL, d BLOB);
     INSERT INTO t VALUES(1,'hello',1.5,x'aabb'),(2,'world',2.25,NULL),(3,NULL,3.75,x'00ff');
     CREATE INDEX it ON t(b);
     CREATE TABLE u(x, y, PRIMARY KEY(x,y)) WITHOUT ROWID;
     INSERT INTO u VALUES('p','q'),('r','s');
     CREATE VIEW v AS SELECT a, b FROM t WHERE c > 2;";

/// Serialize a graphite database and check the image is a valid, correct SQLite
/// file per the sqlite3 CLI.
#[test]
fn serialize_is_valid_sqlite_file() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let mut conn = Connection::open_memory().unwrap();
    conn.execute_batch(SETUP).unwrap();
    let bytes = conn.serialize().unwrap();

    let path = tmp_path("valid");
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, &bytes).unwrap();

    assert_eq!(sqlite3(&path, "PRAGMA integrity_check;"), "ok");
    assert_eq!(
        sqlite3(&path, "SELECT a,b,c,quote(d) FROM t ORDER BY a;"),
        "1|hello|1.5|X'AABB'\n2|world|2.25|NULL\n3||3.75|X'00FF'"
    );
    assert_eq!(sqlite3(&path, "SELECT * FROM u ORDER BY x;"), "p|q\nr|s");
    // The view v = SELECT a,b FROM t WHERE c>2 → rows a=2 (c=2.25) and a=3 (c=3.75).
    assert_eq!(sqlite3(&path, "SELECT * FROM v ORDER BY a;"), "2|world\n3|");
    // The schema survives (tables, index, view all present by name).
    assert_eq!(
        sqlite3(
            &path,
            "SELECT count(*) FROM sqlite_master WHERE name IN ('t','it','u','v');"
        ),
        "4"
    );
    let _ = std::fs::remove_file(&path);
}

/// graphite re-opens its own serialized image and sees the same data.
#[test]
fn serialize_round_trips_in_graphite() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute_batch(SETUP).unwrap();
    let bytes = conn.serialize().unwrap();

    let path = tmp_path("roundtrip");
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, &bytes).unwrap();

    let re = Connection::open(&path).unwrap();
    let r = re.query("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(3));
    let r = re.query("SELECT b FROM t WHERE a=1").unwrap();
    assert_eq!(r.rows[0][0], Value::Text("hello".into()));
    let r = re.query("SELECT count(*) FROM u").unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(2));
    let _ = std::fs::remove_file(&path);
}

/// A WAL-mode database serializes to a standalone image (format versions
/// normalized) that sqlite3 still validates.
#[test]
fn serialize_wal_mode_is_standalone() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let src = tmp_path("wal-src");
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{src}{suffix}"));
    }
    {
        let mut conn = Connection::create(&src).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        conn.execute_batch(SETUP).unwrap();
        let bytes = conn.serialize().unwrap();
        // Page-1 read/write format versions must be 1 (rollback), not 2 (WAL).
        assert_eq!(bytes[18], 1, "read version normalized");
        assert_eq!(bytes[19], 1, "write version normalized");

        let out = tmp_path("wal-out");
        let _ = std::fs::remove_file(&out);
        std::fs::write(&out, &bytes).unwrap();
        assert_eq!(sqlite3(&out, "PRAGMA integrity_check;"), "ok");
        assert_eq!(sqlite3(&out, "SELECT count(*) FROM t;"), "3");
        let _ = std::fs::remove_file(&out);
    }
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{src}{suffix}"));
    }
}

/// The shell's `.backup FILE` writes the same valid image.
#[test]
fn cli_backup_writes_valid_file() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let out = tmp_path("cli-backup");
    let _ = std::fs::remove_file(&out);
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let script = format!("{SETUP}\n.backup {out}\n");
    let child = Command::new(g)
        .arg(":memory:")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .as_ref()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap();

    assert_eq!(sqlite3(&out, "PRAGMA integrity_check;"), "ok");
    assert_eq!(sqlite3(&out, "SELECT count(*) FROM t;"), "3");
    let _ = std::fs::remove_file(&out);
}
