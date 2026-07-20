//! `VACUUM [schema] INTO <file>`: writes a compact copy of the database to a new
//! file. Verified by having `sqlite3` read graphite's output (integrity_check +
//! data + schema), and cross-checked against sqlite3's own VACUUM INTO.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn tmp(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("gsql-vacinto-{}-{name}", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

const SCHEMA: &[&str] = &[
    "CREATE TABLE t(id INTEGER PRIMARY KEY, a UNIQUE, b TEXT)",
    "INSERT INTO t VALUES(1,10,'x'),(2,20,'y'),(3,30,'z')",
    "DELETE FROM t WHERE id=2",
    "CREATE INDEX i ON t(b)",
    "CREATE VIEW v AS SELECT a FROM t WHERE a>5",
    "CREATE TABLE log(m)",
    "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log(m) VALUES(NEW.a); END",
    "PRAGMA user_version=7",
];

fn build(c: &mut Connection) {
    for s in SCHEMA {
        c.execute(s).unwrap();
    }
}

#[test]
fn vacuum_into_is_readable_by_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let out = tmp("out.db");
    let _ = std::fs::remove_file(&out);

    let mut c = Connection::open_memory().unwrap();
    build(&mut c);
    c.execute(&format!("VACUUM INTO '{out}'")).unwrap();

    // sqlite3 reads graphite's output: integrity ok, data + schema intact.
    let q = "PRAGMA integrity_check; SELECT id,a,b FROM t ORDER BY id; \
             SELECT a FROM v ORDER BY a; SELECT type,name FROM sqlite_schema ORDER BY name; \
             PRAGMA user_version";
    let o = Command::new("sqlite3").arg(&out).arg(q).output().unwrap();
    assert!(o.status.success(), "sqlite read failed: {o:?}");
    let got = String::from_utf8_lossy(&o.stdout);
    assert!(got.contains("ok"), "integrity not ok: {got}");
    assert!(
        got.contains("1|10|x") && got.contains("3|30|z"),
        "data: {got}"
    );
    assert!(!got.contains("2|20|y"), "deleted row resurfaced: {got}");
    assert!(got.contains("index|i") && got.contains("view|v") && got.contains("trigger|tr"));
    assert!(got.contains('7'), "user_version not preserved: {got}");

    let _ = std::fs::remove_file(&out);
}

#[test]
fn vacuum_into_matches_sqlite3_logical_content() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let gout = tmp("g.db");
    let sout = tmp("s.db");
    let ssrc = tmp("ssrc.db");
    for p in [&gout, &sout, &ssrc] {
        let _ = std::fs::remove_file(p);
    }

    // graphite builds + VACUUMs INTO gout.
    let mut c = Connection::open_memory().unwrap();
    build(&mut c);
    c.execute(&format!("VACUUM INTO '{gout}'")).unwrap();

    // sqlite builds the same db + VACUUMs INTO sout.
    let script = SCHEMA.join("; ");
    let o = Command::new("sqlite3")
        .arg(&ssrc)
        .arg(format!("{script}; VACUUM INTO '{sout}';"))
        .output()
        .unwrap();
    assert!(o.status.success(), "{o:?}");

    // The two output files must have identical logical content.
    let dump = "SELECT id,a,b FROM t ORDER BY id; SELECT type,name,tbl_name FROM sqlite_schema ORDER BY name; PRAGMA user_version";
    let read = |db: &str| {
        let o = Command::new("sqlite3").arg(db).arg(dump).output().unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    assert_eq!(
        read(&gout),
        read(&sout),
        "graphite vs sqlite VACUUM INTO differ"
    );

    for p in [&gout, &sout, &ssrc] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn vacuum_into_existing_file_errors() {
    // SQLite requires the target to be empty; a non-empty target is refused with a
    // message that depends on whether it opens as a database — verified against the
    // sqlite3 3.50.4 CLI:
    //   * a valid (non-empty) database → "output file already exists";
    //   * any other non-empty content → "file is not a database" (SQLITE_NOTADB);
    //   * an existing *empty* (0-byte) file is written into, not refused.
    let msg = |c: &mut Connection, out: &str| -> String {
        c.execute(&format!("VACUUM INTO '{out}'"))
            .unwrap_err()
            .to_string()
            .trim_start_matches("error: ")
            .to_string()
    };

    // Non-database content → "file is not a database".
    let junk = tmp("exists_junk.db");
    std::fs::write(&junk, b"not empty").unwrap();
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    assert_eq!(msg(&mut c, &junk), "file is not a database");
    let _ = std::fs::remove_file(&junk);

    // A valid, non-empty database → "output file already exists".
    let dbfile = tmp("exists_db.db");
    let _ = std::fs::remove_file(&dbfile);
    {
        let mut seed = Connection::create(&dbfile).unwrap();
        seed.execute("CREATE TABLE z(x)").unwrap();
    }
    assert_eq!(msg(&mut c, &dbfile), "output file already exists");
    let _ = std::fs::remove_file(&dbfile);

    // An existing empty (0-byte) file is acceptable — VACUUM INTO writes into it.
    let empty = tmp("exists_empty.db");
    std::fs::write(&empty, b"").unwrap();
    c.execute(&format!("VACUUM INTO '{empty}'")).unwrap();
    assert!(Connection::open(&empty).is_ok());
    let _ = std::fs::remove_file(&empty);
}

#[test]
fn vacuum_into_roundtrips_back_into_graphite() {
    let out = tmp("rt.db");
    let _ = std::fs::remove_file(&out);
    let mut c = Connection::open_memory().unwrap();
    build(&mut c);
    c.execute(&format!("VACUUM INTO '{out}'")).unwrap();

    // graphite re-opens its own VACUUM INTO output.
    let c2 = Connection::open(&out).unwrap();
    let r = c2.query("SELECT id,a,b FROM t ORDER BY id").unwrap();
    assert_eq!(r.rows.len(), 2);
    let _ = std::fs::remove_file(&out);
}
