//! Write-side compatibility: build a database with graphitesql's low-level write
//! API, then prove (a) graphitesql reads it back and (b) the real `sqlite3` CLI
//! opens it and `PRAGMA integrity_check` passes.
//!
//! The `sqlite3` half is skipped automatically if the CLI is not installed, so
//! the test still exercises the round-trip everywhere.

#![cfg(feature = "std")]

use graphitesql::btree::{create_table_root, insert_table};
use graphitesql::format::encode_record;
use graphitesql::pager::WritePager;
use graphitesql::vfs::{std_file::StdVfs, OpenFlags, Vfs};
use graphitesql::{Connection, Value};
use std::process::Command;

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-wc-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

/// Build `t(a INTEGER PRIMARY KEY, b TEXT, c REAL)` with `n` rows at `path`.
fn build_db(path: &str, n: i64) {
    let vfs = StdVfs::new();
    let _ = vfs.delete(path);
    let _ = vfs.delete(&format!("{path}-journal"));
    let file = vfs.open(path, OpenFlags::READ_WRITE_CREATE).unwrap();
    let journal = vfs
        .open(&format!("{path}-journal"), OpenFlags::READ_WRITE_CREATE)
        .unwrap();
    let mut wp = WritePager::create(file, Some(journal), 4096).unwrap();

    // Create the table's b-tree, then register it in sqlite_schema (page 1).
    let root = create_table_root(&mut wp).unwrap();
    let schema_row = encode_record(&[
        Value::Text("table".into()),
        Value::Text("t".into()),
        Value::Text("t".into()),
        Value::Integer(root as i64),
        Value::Text("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c REAL)".into()),
    ]);
    insert_table(&mut wp, 1, 1, &schema_row).unwrap();

    // Insert rows. Column `a` is the INTEGER PRIMARY KEY, stored as NULL (it
    // aliases the rowid); `b` and `c` carry data.
    for i in 1..=n {
        let row = encode_record(&[
            Value::Null,
            Value::Text(format!("row-{i}")),
            Value::Real(i as f64 + 0.5),
        ]);
        insert_table(&mut wp, root, i, &row).unwrap();
    }
    wp.commit().unwrap();
}

#[test]
fn graphitesql_reads_back_its_own_writes() {
    let path = temp_path("roundtrip.db");
    build_db(&path, 50);

    let c = Connection::open_readonly(&path).unwrap();
    let r = c.query("SELECT a, b, c FROM t ORDER BY a").unwrap();
    assert_eq!(r.rows.len(), 50);
    assert_eq!(r.rows[0][0], Value::Integer(1));
    assert_eq!(r.rows[0][1], Value::Text("row-1".into()));
    assert_eq!(r.rows[49][0], Value::Integer(50));

    let agg = c.query("SELECT count(*), sum(a) FROM t").unwrap();
    assert_eq!(agg.rows[0][0], Value::Integer(50));
    assert_eq!(agg.rows[0][1], Value::Integer(50 * 51 / 2));

    let _ = StdVfs::new().delete(&path);
}

#[test]
fn many_rows_with_splits_read_back() {
    let path = temp_path("splits.db");
    build_db(&path, 1500); // forces leaf + interior splits
    let c = Connection::open_readonly(&path).unwrap();
    let r = c.query("SELECT count(*), min(a), max(a) FROM t").unwrap();
    assert_eq!(
        r.rows[0],
        vec![
            Value::Integer(1500),
            Value::Integer(1),
            Value::Integer(1500)
        ]
    );
    let _ = StdVfs::new().delete(&path);
}

#[test]
fn overflow_delete_reclaims_pages_and_stays_valid() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    use graphitesql::exec::eval::Params;
    let path = temp_path("freelist.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));

    let big = vec![0xCDu8; 30_000]; // spills across several overflow pages
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, data BLOB)")
            .unwrap();
        let p = Params {
            positional: vec![Value::Blob(big.clone())],
            named: vec![],
        };
        c.execute_params("INSERT INTO t(data) VALUES (?1)", &p)
            .unwrap();
        // Deleting the overflow row must free its overflow pages onto the freelist.
        assert_eq!(c.execute("DELETE FROM t WHERE id = 1").unwrap(), 1);
        // Reinsert; the freed pages should be reused.
        c.execute_params("INSERT INTO t(data) VALUES (?1)", &p)
            .unwrap();
    }

    let run = |sql: &str| {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg(sql)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(run("PRAGMA integrity_check;"), "ok");
    assert_eq!(run("SELECT count(*) FROM t;"), "1");
    assert_eq!(run("SELECT length(data) FROM t;"), "30000");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn sqlite3_cli_opens_graphitesql_database() {
    // Skip cleanly if the sqlite3 CLI is unavailable.
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping cross-engine compatibility check");
        return;
    }

    let path = temp_path("forsqlite.db");
    build_db(&path, 200);

    let run = |sql: &str| -> String {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg(sql)
            .output()
            .expect("run sqlite3");
        assert!(
            out.status.success(),
            "sqlite3 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // The compatibility gate: SQLite's own integrity check must pass.
    assert_eq!(run("PRAGMA integrity_check;"), "ok");
    assert_eq!(run("SELECT count(*) FROM t;"), "200");
    assert_eq!(run("SELECT b FROM t WHERE a = 1;"), "row-1");
    assert_eq!(run("SELECT a FROM t ORDER BY a DESC LIMIT 1;"), "200");
    // Round-trip a value through SQLite's own decoder.
    assert_eq!(run("SELECT c FROM t WHERE a = 2;"), "2.5");

    let _ = StdVfs::new().delete(&path);
}
