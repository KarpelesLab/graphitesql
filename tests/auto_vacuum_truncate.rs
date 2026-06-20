//! Roadmap C6b-3: FULL `auto_vacuum` commit-time truncation.
//!
//! After deleting a large fraction of rows from a FULL auto_vacuum database,
//! graphite relocates trailing pages into freed lower slots and truncates the
//! file, so it shrinks instead of leaving holes. These tests verify against the
//! pinned `sqlite3` oracle that the shrunken file still passes
//! `PRAGMA integrity_check`, that the page count actually dropped, that every
//! surviving row (including overflow values and secondary-index lookups) reads
//! back, and that the round trip works in both directions (graphite→sqlite and
//! sqlite→graphite→sqlite). The sqlite3 assertions are skipped gracefully when
//! no `sqlite3` is on PATH, following `tests/auto_vacuum_write.rs`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn have_sqlite3() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn sqlite3(path: &str, sql: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(path)
        .arg(sql)
        .output()
        .expect("spawn sqlite3");
    assert!(
        out.status.success(),
        "sqlite3 {sql:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Feed a large script to sqlite3 over stdin (avoids the argv length limit).
fn sqlite3_script(path: &str, sql: &str) {
    use std::io::Write;
    let mut child = Command::new("sqlite3")
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sqlite3");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "sqlite3 script failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tmp_path(tag: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-avt-{}-{}.db", tag, std::process::id()));
    let path = p.to_string_lossy().into_owned();
    cleanup(&path);
    path
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

fn page_count(path: &str) -> i64 {
    let c = Connection::open(path).unwrap();
    match c.query("PRAGMA page_count").unwrap().rows[0][0] {
        Value::Integer(n) => n,
        ref v => panic!("page_count not an integer: {v:?}"),
    }
}

/// Headline acceptance test: build a FULL auto_vacuum file spanning several
/// pages (overflow chains + a secondary index), delete a large fraction, commit,
/// then assert the file is meaningfully smaller, integrity_check is ok, and all
/// surviving rows + index lookups read back correctly.
#[test]
fn full_auto_vacuum_commit_truncates_and_stays_sound() {
    let path = tmp_path("shrink");

    let big: String = "Z".repeat(4500); // every row spills to an overflow page
    let huge: String = "Q".repeat(50_000); // a multi-page overflow chain

    let kept: i64;
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("PRAGMA auto_vacuum=FULL").unwrap();
        c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
            .unwrap();
        for i in 1..=1000i64 {
            c.execute(&format!("INSERT INTO t VALUES({i}, '{big}')"))
                .unwrap();
        }
        c.execute(&format!("INSERT INTO t VALUES(100000, '{huge}')"))
            .unwrap();
        c.execute("CREATE INDEX ix ON t(b)").unwrap();
    }
    let before = page_count(&path);

    {
        let mut c = Connection::open(&path).unwrap();
        // Delete a large fraction (~80%) to free many trailing pages.
        c.execute("DELETE FROM t WHERE a <= 800").unwrap();
        kept = match c.query("SELECT count(*) FROM t").unwrap().rows[0][0] {
            Value::Integer(n) => n,
            ref v => panic!("count: {v:?}"),
        };
        assert_eq!(kept, 201); // 1000 + 1 huge - 800 deleted

        // Same-session reads after the relocating commit must still work: the
        // relocator never moves root pages, so the executor's cached schema
        // rootpages stay valid.
        assert_eq!(
            c.query("SELECT length(b) FROM t WHERE a=100000")
                .unwrap()
                .rows[0][0],
            Value::Integer(50_000)
        );
        assert_eq!(
            c.query(&format!("SELECT a FROM t WHERE b='{big}' AND a=999"))
                .unwrap()
                .rows[0][0],
            Value::Integer(999)
        );
        assert_eq!(
            c.query("PRAGMA integrity_check").unwrap().rows[0][0],
            Value::Text("ok".into())
        );
    }
    let after = page_count(&path);

    // (b) The file shrank meaningfully.
    assert!(
        after < before,
        "expected truncation to shrink the file: before={before} after={after}"
    );
    // A large delete should reclaim a big chunk, not a token page or two.
    assert!(
        after * 2 < before,
        "expected the file to roughly halve at least: before={before} after={after}"
    );

    // (c)+(d) graphite reads its own shrunken file back correctly.
    {
        let c = Connection::open(&path).unwrap();
        assert_eq!(
            c.query("PRAGMA auto_vacuum").unwrap().rows[0][0],
            Value::Integer(1)
        );
        assert_eq!(
            c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
            Value::Integer(kept)
        );
        assert_eq!(
            c.query("SELECT length(b) FROM t WHERE a=100000")
                .unwrap()
                .rows[0][0],
            Value::Integer(50_000)
        );
        // An index lookup of a surviving row works.
        assert_eq!(
            c.query(&format!("SELECT a FROM t WHERE b='{big}' AND a=900"))
                .unwrap()
                .rows[0][0],
            Value::Integer(900)
        );
        assert_eq!(
            c.query("PRAGMA integrity_check").unwrap().rows[0][0],
            Value::Text("ok".into())
        );
    }

    // (a)+(d) sqlite3 confirms the same on the shrunken file.
    if have_sqlite3() {
        assert_eq!(
            sqlite3(&path, "PRAGMA integrity_check;"),
            "ok",
            "sqlite3 integrity_check on a graphite-truncated FULL auto_vacuum db"
        );
        assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "1");
        assert_eq!(sqlite3(&path, "SELECT count(*) FROM t;"), kept.to_string());
        assert_eq!(
            sqlite3(&path, "SELECT length(b) FROM t WHERE a=100000;"),
            "50000"
        );
        // Index-driven lookup through sqlite on a surviving row.
        assert_eq!(
            sqlite3(
                &path,
                &format!("SELECT a FROM t WHERE b='{big}' AND a=950;")
            ),
            "950"
        );
        // The deleted rows are gone.
        assert_eq!(sqlite3(&path, "SELECT count(*) FROM t WHERE a<=800;"), "0");
        // sqlite agrees on the smaller page count.
        assert_eq!(
            sqlite3(&path, "PRAGMA page_count;").parse::<i64>().unwrap(),
            after
        );
    }

    cleanup(&path);
}

/// A sqlite-created FULL file that graphite deletes-from then truncates,
/// round-tripped back to sqlite.
#[test]
fn sqlite_created_full_db_truncated_by_graphite() {
    if !have_sqlite3() {
        return;
    }
    let path = tmp_path("sqlite_src");
    let big: String = "Z".repeat(4500);

    // sqlite3 builds the FULL auto_vacuum database with overflow + an index.
    let mut build = String::from("PRAGMA auto_vacuum=FULL;\nPRAGMA page_size=4096;\n");
    build.push_str("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\n");
    build.push_str("BEGIN;\n");
    for i in 1..=600i64 {
        build.push_str(&format!("INSERT INTO t VALUES({i}, '{big}');\n"));
    }
    build.push_str("COMMIT;\n");
    build.push_str("CREATE INDEX ix ON t(b);\n");
    sqlite3_script(&path, &build);
    assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "1");
    let before = page_count(&path);

    // graphite deletes a large fraction and truncates on commit.
    let kept: i64 = {
        let mut c = Connection::open(&path).unwrap();
        c.execute("DELETE FROM t WHERE a <= 480").unwrap();
        match c.query("SELECT count(*) FROM t").unwrap().rows[0][0] {
            Value::Integer(n) => n,
            ref v => panic!("count: {v:?}"),
        }
    };
    assert_eq!(kept, 120);
    let after = page_count(&path);
    assert!(after < before, "before={before} after={after}");

    // sqlite reads the shrunken file back: sound, correct, still FULL.
    assert_eq!(sqlite3(&path, "PRAGMA integrity_check;"), "ok");
    assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "1");
    assert_eq!(sqlite3(&path, "SELECT count(*) FROM t;"), kept.to_string());
    assert_eq!(
        sqlite3(
            &path,
            &format!("SELECT a FROM t WHERE b='{big}' AND a=600;")
        ),
        "600"
    );
    assert_eq!(
        sqlite3(&path, "PRAGMA page_count;").parse::<i64>().unwrap(),
        after
    );

    cleanup(&path);
}

/// A NONE (default) auto_vacuum database must never be truncated/relocated:
/// deletes leave the file the same size (freed pages stay on the freelist).
#[test]
fn none_auto_vacuum_is_not_truncated() {
    let path = tmp_path("none");
    let big: String = "Z".repeat(4500);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
            .unwrap();
        for i in 1..=400i64 {
            c.execute(&format!("INSERT INTO t VALUES({i}, '{big}')"))
                .unwrap();
        }
    }
    let before = page_count(&path);
    {
        let mut c = Connection::open(&path).unwrap();
        c.execute("DELETE FROM t WHERE a <= 300").unwrap();
    }
    let after = page_count(&path);
    assert_eq!(after, before, "NONE auto_vacuum must not shrink the file");
    assert_eq!(
        Connection::open(&path)
            .unwrap()
            .query("PRAGMA auto_vacuum")
            .unwrap()
            .rows[0][0],
        Value::Integer(0)
    );
    cleanup(&path);
}

/// INCREMENTAL auto_vacuum must also not auto-truncate on commit (only FULL does;
/// INCREMENTAL reclaims via `PRAGMA incremental_vacuum`, unimplemented here).
#[test]
fn incremental_auto_vacuum_is_not_truncated_on_commit() {
    let path = tmp_path("incr");
    let big: String = "Z".repeat(4500);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("PRAGMA auto_vacuum=INCREMENTAL").unwrap();
        c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
            .unwrap();
        for i in 1..=400i64 {
            c.execute(&format!("INSERT INTO t VALUES({i}, '{big}')"))
                .unwrap();
        }
    }
    let before = page_count(&path);
    {
        let mut c = Connection::open(&path).unwrap();
        c.execute("DELETE FROM t WHERE a <= 300").unwrap();
    }
    let after = page_count(&path);
    assert_eq!(
        after, before,
        "INCREMENTAL must not auto-truncate on commit"
    );
    if have_sqlite3() {
        assert_eq!(sqlite3(&path, "PRAGMA integrity_check;"), "ok");
        assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "2");
    }
    cleanup(&path);
}
