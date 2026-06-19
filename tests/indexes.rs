//! Phase 9: CREATE INDEX, index maintenance on INSERT/UPDATE/DELETE, and DROP.
//!
//! The decisive check is real `sqlite3`'s `PRAGMA integrity_check`, which
//! verifies that every index entry corresponds to a table row and that the
//! counts match — so a passing check means our index b-trees are correct.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-idx-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite3_run(path: &str, sql: &str) -> String {
    let out = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn create_index_maintenance_and_integrity() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let path = temp_path("idx.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, age INT)")
            .unwrap();
        // Insert enough rows that the index b-tree needs interior pages.
        for i in 1..=500 {
            c.execute(&format!(
                "INSERT INTO t(name, age) VALUES ('name-{:04}', {})",
                (500 - i), // names inserted out of sorted order
                i % 50
            ))
            .unwrap();
        }
        // Build one index over existing data (populated by scan) ...
        c.execute("CREATE INDEX idx_name ON t(name)").unwrap();
        // ... and use incremental maintenance for rows inserted afterwards.
        c.execute("CREATE INDEX idx_age ON t(age)").unwrap();
        for i in 501..=560 {
            c.execute(&format!(
                "INSERT INTO t(name, age) VALUES ('name-{i:04}', {})",
                i % 50
            ))
            .unwrap();
        }
        // Mutations trigger index rebuilds.
        c.execute("DELETE FROM t WHERE age = 0").unwrap();
        c.execute("UPDATE t SET name = 'updated' WHERE id <= 5")
            .unwrap();
    }

    // SQLite must agree the database (incl. both indexes) is internally consistent.
    assert_eq!(sqlite3_run(&path, "PRAGMA integrity_check;"), "ok");

    // And a few index-driven lookups must return the right answers.
    let our_count = {
        let c = Connection::open_readonly(&path).unwrap();
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0].clone()
    };
    let sqlite_count = sqlite3_run(&path, "SELECT count(*) FROM t;");
    assert_eq!(our_count, Value::Integer(sqlite_count.parse().unwrap()));

    // sqlite3 uses idx_name here; the updated rows must be found.
    assert_eq!(
        sqlite3_run(&path, "SELECT count(*) FROM t WHERE name = 'updated';"),
        "5"
    );

    cleanup(&path);
}

#[test]
fn drop_index_and_table() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let path = temp_path("drop.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE a(id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        c.execute("CREATE INDEX idx_v ON a(v)").unwrap();
        c.execute("CREATE TABLE b(id INTEGER PRIMARY KEY)").unwrap();
        for i in 1..=100 {
            c.execute(&format!("INSERT INTO a(v) VALUES ('v{i}')"))
                .unwrap();
        }

        // Drop the index; the table remains and stays consistent.
        c.execute("DROP INDEX idx_v").unwrap();
        assert!(c.schema().index("idx_v").is_none());

        // Drop a whole table (with its rows); the other table is untouched.
        c.execute("DROP TABLE a").unwrap();
        assert!(c.schema().table("a").is_none());
        assert!(c.schema().table("b").is_some());
    }
    assert_eq!(sqlite3_run(&path, "PRAGMA integrity_check;"), "ok");
    assert_eq!(
        sqlite3_run(&path, "SELECT count(*) FROM sqlite_schema WHERE name='a';"),
        "0"
    );
    cleanup(&path);
}

#[test]
fn index_equality_lookup_results() {
    // Verify index-driven equality lookups return correct rows (single and
    // composite leftmost-prefix), matching a full scan.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, g INT, s TEXT)")
        .unwrap();
    for i in 1..=300 {
        c.execute(&format!(
            "INSERT INTO t(a, g, s) VALUES ({}, {}, 'v{}')",
            i,
            i % 7,
            i % 4
        ))
        .unwrap();
    }
    c.execute("CREATE INDEX ia ON t(a)").unwrap();
    c.execute("CREATE INDEX igs ON t(g, s)").unwrap();

    // Single-column index equality.
    let r = c.query("SELECT id FROM t WHERE a = 150").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(150)]]);

    // Index + an extra predicate the index doesn't cover (still correct).
    let r = c.query("SELECT count(*) FROM t WHERE g = 3").unwrap();
    let expected_g3 = (1..=300).filter(|i| i % 7 == 3).count() as i64;
    assert_eq!(r.rows[0][0], Value::Integer(expected_g3));

    // Composite index leftmost prefix (g) + full key (g, s).
    let r = c
        .query("SELECT count(*) FROM t WHERE g = 2 AND s = 'v1'")
        .unwrap();
    let expected = (1..=300).filter(|i| i % 7 == 2 && i % 4 == 1).count() as i64;
    assert_eq!(r.rows[0][0], Value::Integer(expected));

    // Affinity: querying an INT column with a text literal still hits the index.
    let r = c.query("SELECT id FROM t WHERE a = '42'").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(42)]]);

    // After deleting the row, the index lookup no longer finds it.
    c.execute("DELETE FROM t WHERE a = 150").unwrap();
    assert!(c
        .query("SELECT id FROM t WHERE a = 150")
        .unwrap()
        .rows
        .is_empty());
}

#[test]
fn drop_table_if_exists_is_noop() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("DROP TABLE IF EXISTS nope").unwrap();
    assert!(c.execute("DROP TABLE nope").is_err());
}
