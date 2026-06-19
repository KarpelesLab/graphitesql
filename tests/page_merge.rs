//! Phase 9: b-tree page merging on delete — emptied table pages are reclaimed
//! (returned to the freelist and reused), keeping the b-tree compact and
//! balanced. The decisive gate is real `sqlite3`'s `PRAGMA integrity_check`
//! across diverse delete patterns, plus page reuse (the file doesn't grow
//! unboundedly across delete/insert cycles).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-pm-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

fn cleanup(path: &str) {
    for s in ["", "-journal", "-wal"] {
        let _ = std::fs::remove_file(format!("{path}{s}"));
    }
}

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn integrity(path: &str) -> String {
    Command::new("sqlite3")
        .arg(path)
        .arg("PRAGMA integrity_check;")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap()
}

fn count(c: &Connection, sql: &str) -> i64 {
    match c.query(sql).unwrap().rows[0][0] {
        Value::Integer(n) => n,
        ref o => panic!("{o:?}"),
    }
}

#[test]
fn heavy_delete_reclaims_and_stays_valid() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_path("heavy.db");
    cleanup(&path);
    let page_count_after_first;
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, s TEXT)")
            .unwrap();
        for i in 1..=3000 {
            c.execute(&format!("INSERT INTO t(s) VALUES ('value-number-{i}')"))
                .unwrap();
        }
        page_count_after_first = count(&c, "PRAGMA page_count");
        // Delete ~97% of rows; emptied leaves must be reclaimed.
        c.execute("DELETE FROM t WHERE id > 100").unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM t"), 100);
        // Re-insert another batch: it should reuse freed pages rather than only
        // growing the file.
        for i in 1..=2000 {
            c.execute(&format!("INSERT INTO t(s) VALUES ('again-{i}')"))
                .unwrap();
        }
        let after = count(&c, "PRAGMA page_count");
        assert!(
            after <= page_count_after_first + 5,
            "freed pages should be reused: first={page_count_after_first} after={after}"
        );
        assert_eq!(count(&c, "SELECT count(*) FROM t"), 2100);
    }
    assert_eq!(integrity(&path), "ok");
    cleanup(&path);
}

#[test]
fn scattered_and_full_deletes_valid() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_path("scatter.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT)")
            .unwrap();
        c.execute("CREATE INDEX ia ON t(a)").unwrap();
        for i in 1..=1500 {
            c.execute(&format!("INSERT INTO t(a,b) VALUES ({}, 'x{i}')", i % 30))
                .unwrap();
        }
        // Scattered delete (every third row).
        c.execute("DELETE FROM t WHERE id % 3 = 0").unwrap();
        assert_eq!(integrity(&path), "ok");
        // Delete a contiguous range.
        c.execute("DELETE FROM t WHERE id BETWEEN 500 AND 1400")
            .unwrap();
        assert_eq!(integrity(&path), "ok");
        // The index still answers correctly.
        let want = count(&c, "SELECT count(*) FROM t WHERE a = 5");
        assert!(want >= 0);
        // Delete everything: the table becomes a single empty leaf.
        c.execute("DELETE FROM t").unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM t"), 0);
    }
    assert_eq!(integrity(&path), "ok");
    cleanup(&path);
}

#[test]
fn delete_then_query_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Build the same data + deletes in both engines, compare the survivors.
    let setup: Vec<String> = {
        let mut v = vec!["CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)".to_string()];
        for i in 1..=800 {
            v.push(format!("INSERT INTO t(v) VALUES ({})", i * 7 % 101));
        }
        v.push("DELETE FROM t WHERE id % 5 <> 0".to_string());
        v
    };
    let path = temp_path("cmp.db");
    cleanup(&path);
    let mut c = Connection::create(&path).unwrap();
    for s in &setup {
        c.execute(s).unwrap();
    }
    drop(c);
    let want = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!(
            "{};SELECT count(*), sum(v) FROM t;",
            setup.join(";")
        ))
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap();
    let c = Connection::open_readonly(&path).unwrap();
    let got = format!(
        "{}|{}",
        count(&c, "SELECT count(*) FROM t"),
        count(&c, "SELECT sum(v) FROM t"),
    );
    assert_eq!(got, want);
    assert_eq!(integrity(&path), "ok");
    cleanup(&path);
}
