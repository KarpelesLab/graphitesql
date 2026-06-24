//! Roadmap C6b-4: `PRAGMA incremental_vacuum(N)` for `auto_vacuum=INCREMENTAL`.
//!
//! For INCREMENTAL databases graphite never auto-truncates on commit (free pages
//! accumulate); the application reclaims them on demand with
//! `PRAGMA incremental_vacuum`. These tests verify against the pinned `sqlite3`
//! oracle that after deleting a large fraction of rows, an unbounded
//! `PRAGMA incremental_vacuum` shrinks the file to near-minimal and a bounded
//! `PRAGMA incremental_vacuum(N)` reclaims about `min(N, freelist)` pages, that
//! the result always passes `PRAGMA integrity_check`, that `freelist_count` drops
//! accordingly, that every surviving row + index lookup reads back, and that the
//! round trip works in both directions (graphite→sqlite and sqlite→graphite→
//! sqlite). NONE/FULL must be byte-identical to before (no INCREMENTAL path).
//! The sqlite3 assertions are skipped gracefully when no `sqlite3` is on PATH,
//! following `tests/auto_vacuum_truncate.rs`.

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
    p.push(format!("graphitesql-iv-{}-{}.db", tag, std::process::id()));
    let path = p.to_string_lossy().into_owned();
    cleanup(&path);
    path
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

fn int_pragma(c: &Connection, sql: &str) -> i64 {
    match c.query(sql).unwrap().rows[0][0] {
        Value::Integer(n) => n,
        ref v => panic!("{sql} not an integer: {v:?}"),
    }
}

fn page_count(path: &str) -> i64 {
    let c = Connection::open(path).unwrap();
    int_pragma(&c, "PRAGMA page_count")
}

fn freelist_count(path: &str) -> i64 {
    let c = Connection::open(path).unwrap();
    int_pragma(&c, "PRAGMA freelist_count")
}

/// Headline acceptance test: build an INCREMENTAL auto_vacuum file spanning many
/// pages (overflow chains + a secondary index), delete a large fraction so free
/// pages accumulate, confirm commit does NOT shrink the file, then run an
/// unbounded `PRAGMA incremental_vacuum` and assert the file shrank to
/// near-minimal, the freelist drained, integrity is ok, and all surviving rows +
/// index lookups still read back through both graphite and sqlite3.
#[test]
fn unbounded_incremental_vacuum_reclaims_and_stays_sound() {
    let path = tmp_path("unbounded");

    let big: String = "Z".repeat(4500); // every row spills to an overflow page
    let huge: String = "Q".repeat(50_000); // a multi-page overflow chain

    let kept: i64;
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("PRAGMA auto_vacuum=INCREMENTAL").unwrap();
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

    // Delete ~80% so many trailing pages become free.
    {
        let mut c = Connection::open(&path).unwrap();
        c.execute("DELETE FROM t WHERE a <= 800").unwrap();
        kept = int_pragma(&c, "SELECT count(*) FROM t");
        assert_eq!(kept, 201); // 1000 + 1 huge - 800 deleted
    }

    // The DELETE alone must NOT have shrunk the file: free pages just accumulate.
    let after_delete = page_count(&path);
    let free_after_delete = freelist_count(&path);
    assert!(
        free_after_delete > 0,
        "expected free pages to accumulate after the delete"
    );

    // Now reclaim everything possible.
    {
        let mut c = Connection::open(&path).unwrap();
        c.execute("PRAGMA incremental_vacuum").unwrap();
        // Same-session reads after the relocating reclamation must still work.
        assert_eq!(
            c.query("SELECT length(b) FROM t WHERE a=100000")
                .unwrap()
                .rows[0][0],
            Value::Integer(50_000)
        );
        assert_eq!(
            c.query("PRAGMA integrity_check").unwrap().rows[0][0],
            Value::Text("ok".into())
        );
    }
    let after_vacuum = page_count(&path);
    let free_after_vacuum = freelist_count(&path);

    // (b) The file shrank meaningfully; (c) the freelist drained.
    assert!(
        after_vacuum < after_delete,
        "incremental_vacuum should shrink the file: {after_delete} -> {after_vacuum}"
    );
    assert!(
        after_vacuum * 2 < after_delete,
        "expected the file to roughly halve at least: {after_delete} -> {after_vacuum}"
    );
    assert!(
        free_after_vacuum < free_after_delete,
        "freelist should drop: {free_after_delete} -> {free_after_vacuum}"
    );

    // (d) graphite reads its own reclaimed file back correctly.
    {
        let c = Connection::open(&path).unwrap();
        assert_eq!(int_pragma(&c, "PRAGMA auto_vacuum"), 2); // (e)
        assert_eq!(int_pragma(&c, "SELECT count(*) FROM t"), kept);
        assert_eq!(
            c.query("SELECT length(b) FROM t WHERE a=100000")
                .unwrap()
                .rows[0][0],
            Value::Integer(50_000)
        );
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

    // (a)+(d)+(e) sqlite3 confirms the same on the reclaimed file.
    if have_sqlite3() {
        assert_eq!(
            sqlite3(&path, "PRAGMA integrity_check;"),
            "ok",
            "sqlite3 integrity_check on a graphite-reclaimed INCREMENTAL db"
        );
        assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "2");
        assert_eq!(sqlite3(&path, "SELECT count(*) FROM t;"), kept.to_string());
        assert_eq!(
            sqlite3(&path, "SELECT length(b) FROM t WHERE a=100000;"),
            "50000"
        );
        assert_eq!(
            sqlite3(
                &path,
                &format!("SELECT a FROM t WHERE b='{big}' AND a=950;")
            ),
            "950"
        );
        assert_eq!(sqlite3(&path, "SELECT count(*) FROM t WHERE a<=800;"), "0");
        assert_eq!(
            sqlite3(&path, "PRAGMA page_count;").parse::<i64>().unwrap(),
            after_vacuum
        );
    }

    cleanup(&path);
}

/// Bounded `PRAGMA incremental_vacuum(N)` reclaims about `min(N, freelist)` pages,
/// stays sound, and a second unbounded call drains the rest. The call-form
/// `incremental_vacuum(N)` and the `= N` form behave identically.
#[test]
fn bounded_incremental_vacuum_reclaims_at_most_n() {
    let path = tmp_path("bounded");
    let big: String = "Z".repeat(4500);

    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("PRAGMA auto_vacuum=INCREMENTAL").unwrap();
        c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
            .unwrap();
        for i in 1..=600i64 {
            c.execute(&format!("INSERT INTO t VALUES({i}, '{big}')"))
                .unwrap();
        }
    }
    {
        let mut c = Connection::open(&path).unwrap();
        c.execute("DELETE FROM t WHERE a <= 480").unwrap();
    }
    let before = page_count(&path);
    let free_before = freelist_count(&path);
    assert!(free_before >= 5, "need >=5 free pages, got {free_before}");

    // Reclaim at most 5 pages off the end.
    {
        let mut c = Connection::open(&path).unwrap();
        c.execute("PRAGMA incremental_vacuum(5)").unwrap();
    }
    let after5 = page_count(&path);
    let dropped = before - after5;
    // At most 5 pages off the end; at least 1 (there is plenty free below).
    assert!(
        (1..=5).contains(&dropped),
        "bounded(5) should drop 1..=5 pages, dropped {dropped} ({before} -> {after5})"
    );
    assert!(
        after5 > 1 && freelist_count(&path) > 0,
        "bounded vacuum must not over-reclaim: {after5} pages, freelist now {}",
        freelist_count(&path)
    );
    {
        let c = Connection::open(&path).unwrap();
        assert_eq!(
            c.query("PRAGMA integrity_check").unwrap().rows[0][0],
            Value::Text("ok".into())
        );
    }

    // A following unbounded call drains the rest to near-minimal.
    {
        let mut c = Connection::open(&path).unwrap();
        c.execute("PRAGMA incremental_vacuum = 0").unwrap();
    }
    let after_full = page_count(&path);
    assert!(
        after_full < after5,
        "unbounded follow-up should reclaim more: {after5} -> {after_full}"
    );

    if have_sqlite3() {
        assert_eq!(sqlite3(&path, "PRAGMA integrity_check;"), "ok");
        assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "2");
        assert_eq!(sqlite3(&path, "SELECT count(*) FROM t;"), "120");
        assert_eq!(sqlite3(&path, "SELECT count(*) FROM t WHERE a<=480;"), "0");
        assert_eq!(
            sqlite3(&path, "PRAGMA page_count;").parse::<i64>().unwrap(),
            after_full
        );
    }

    cleanup(&path);
}

/// A sqlite-created INCREMENTAL file that graphite deletes-from then reclaims via
/// `PRAGMA incremental_vacuum`, round-tripped back to sqlite.
#[test]
fn sqlite_created_incremental_db_reclaimed_by_graphite() {
    if !have_sqlite3() {
        return;
    }
    let path = tmp_path("sqlite_src");
    let big: String = "Z".repeat(4500);

    let mut build = String::from("PRAGMA auto_vacuum=INCREMENTAL;\nPRAGMA page_size=4096;\n");
    build.push_str("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\n");
    build.push_str("BEGIN;\n");
    for i in 1..=600i64 {
        build.push_str(&format!("INSERT INTO t VALUES({i}, '{big}');\n"));
    }
    build.push_str("COMMIT;\n");
    build.push_str("CREATE INDEX ix ON t(b);\n");
    sqlite3_script(&path, &build);
    assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "2");

    let kept: i64 = {
        let mut c = Connection::open(&path).unwrap();
        c.execute("DELETE FROM t WHERE a <= 480").unwrap();
        int_pragma(&c, "SELECT count(*) FROM t")
    };
    assert_eq!(kept, 120);
    let before = page_count(&path);

    {
        let mut c = Connection::open(&path).unwrap();
        c.execute("PRAGMA incremental_vacuum").unwrap();
    }
    let after = page_count(&path);
    assert!(after < before, "before={before} after={after}");

    // sqlite reads the reclaimed file back: sound, correct, still INCREMENTAL.
    assert_eq!(sqlite3(&path, "PRAGMA integrity_check;"), "ok");
    assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "2");
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

/// `PRAGMA incremental_vacuum` is a no-op for NONE and FULL: the file is not
/// changed by the pragma (NONE keeps freed pages, FULL already compacted on
/// commit), matching SQLite which does nothing for those modes.
#[test]
fn incremental_vacuum_is_noop_for_none_and_full() {
    // NONE: deletes leave free pages; incremental_vacuum must not touch them.
    {
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
        {
            let mut c = Connection::open(&path).unwrap();
            c.execute("DELETE FROM t WHERE a <= 300").unwrap();
        }
        let before = page_count(&path);
        {
            let mut c = Connection::open(&path).unwrap();
            c.execute("PRAGMA incremental_vacuum").unwrap();
        }
        assert_eq!(
            page_count(&path),
            before,
            "incremental_vacuum must be a no-op for NONE"
        );
        if have_sqlite3() {
            assert_eq!(sqlite3(&path, "PRAGMA integrity_check;"), "ok");
            assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "0");
        }
        cleanup(&path);
    }

    // FULL: already truncates on commit; incremental_vacuum changes nothing more.
    {
        let path = tmp_path("full");
        let big: String = "Z".repeat(4500);
        {
            let mut c = Connection::create(&path).unwrap();
            c.execute("PRAGMA auto_vacuum=FULL").unwrap();
            c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
                .unwrap();
            for i in 1..=400i64 {
                c.execute(&format!("INSERT INTO t VALUES({i}, '{big}')"))
                    .unwrap();
            }
        }
        {
            let mut c = Connection::open(&path).unwrap();
            c.execute("DELETE FROM t WHERE a <= 300").unwrap();
        }
        let before = page_count(&path);
        {
            let mut c = Connection::open(&path).unwrap();
            c.execute("PRAGMA incremental_vacuum").unwrap();
        }
        assert_eq!(
            page_count(&path),
            before,
            "incremental_vacuum must be a no-op for FULL (already compacted)"
        );
        if have_sqlite3() {
            assert_eq!(sqlite3(&path, "PRAGMA integrity_check;"), "ok");
            assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "1");
        }
        cleanup(&path);
    }
}

/// The bare and `(N)` forms run as a *write*, so on the read-only `query()` path
/// they return an `Unsupported("…use execute()")` signal — the CLI retries on it
/// to route `PRAGMA incremental_vacuum` / `(N)` to the mutating path. (The `= N`
/// form already routes to execute() directly.) `execute()` runs every form.
#[test]
fn incremental_vacuum_query_path_signals_use_execute() {
    let c = Connection::open_memory().unwrap();
    for form in ["PRAGMA incremental_vacuum", "PRAGMA incremental_vacuum(5)"] {
        match c.query(form) {
            Err(graphitesql::Error::Unsupported(m)) => {
                assert!(m.contains("use execute()"), "unexpected message: {m}");
            }
            other => panic!("expected Unsupported(use execute()) for {form}, got {other:?}"),
        }
    }
    // execute() accepts all three forms on an INCREMENTAL database.
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA auto_vacuum=INCREMENTAL").unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("PRAGMA incremental_vacuum").unwrap();
    c.execute("PRAGMA incremental_vacuum(3)").unwrap();
    c.execute("PRAGMA incremental_vacuum = 3").unwrap();
}
