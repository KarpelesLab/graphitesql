//! Regression: a delete-heavy FTS5 workload must leave a file stock `sqlite3`
//! can open — `PRAGMA quick_check` = `ok`.
//!
//! Before the fix, the vtab store's per-row `_content` deletes went straight to
//! `delete_table` with no page-merge/compaction step, so an emptied leaf below
//! the `f_content` interior root was left in place. A non-root leaf with zero
//! cells is a *malformed* sqlite b-tree: graphite's own (count-based)
//! `integrity_check` said `ok`, but `sqlite3` reported "database disk image is
//! malformed". The fix compacts the backing `_content`/`_data` b-tree after a
//! vtab store batch (the same page-merge-on-delete the ordinary DELETE path
//! does), and `integrity_check` now also structurally walks every b-tree so it
//! detects such a malformation.
//!
//! This test drives several delete-heavy shapes through graphite and asserts
//! `sqlite3 quick_check` = `ok`, graphite `integrity_check` = `ok`, and the row
//! set matches sqlite. Skipped when `sqlite3` (with FTS5) is not on PATH.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-valid-{}-{}-{}.db",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let p = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&p);
    p
}

fn have_fts5_sqlite() -> bool {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg("CREATE VIRTUAL TABLE t USING fts5(a); SELECT 1;")
        .output();
    matches!(o, Ok(o) if o.status.success())
}

/// `sqlite3 <path> "PRAGMA quick_check"` — returns the stdout, or the stderr on
/// failure (so a malformed-file error is captured, not panicked on).
fn sqlite_quick_check(path: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(path)
        .arg("PRAGMA quick_check;")
        .output()
        .unwrap();
    if o.status.success() {
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    } else {
        format!("ERR: {}", String::from_utf8_lossy(&o.stderr).trim())
    }
}

fn graphite_scalar(c: &Connection, q: &str) -> String {
    let r = c.query(q).unwrap();
    match r.rows.first().and_then(|row| row.first()) {
        Some(graphitesql::Value::Text(t)) => String::from(t.as_str()),
        Some(graphitesql::Value::Integer(i)) => i.to_string(),
        other => format!("{other:?}"),
    }
}

/// Insert `ins` documents (rowid 1..=ins), then delete rowid 1..=del. Assert the
/// resulting graphite file is a valid sqlite database and the surviving row set
/// matches.
fn assert_delete_heavy_valid(tag: &str, ins: i64, del: i64) {
    let path = tmp_path(tag);
    let mut c = Connection::create(&path).unwrap();
    c.execute("CREATE VIRTUAL TABLE f USING fts5(a);").unwrap();
    for i in 1..=ins {
        c.execute(&format!(
            "INSERT INTO f(rowid,a) VALUES({i},'doc{i} term{i} shared word{} extra{} fill{}');",
            i % 7,
            i % 13,
            i % 5
        ))
        .unwrap();
    }
    for i in 1..=del {
        c.execute(&format!("DELETE FROM f WHERE rowid={i};"))
            .unwrap();
    }
    drop(c);

    // sqlite must be able to open and structurally validate the file.
    let qc = sqlite_quick_check(&path);
    assert_eq!(
        qc, "ok",
        "sqlite quick_check rejected graphite's file for {tag}"
    );

    // graphite's own integrity_check must also be clean (and now structural).
    let c = Connection::open(&path).unwrap();
    assert_eq!(
        graphite_scalar(&c, "PRAGMA integrity_check;"),
        "ok",
        "graphite integrity_check not ok for {tag}"
    );

    // Surviving row set agrees with sqlite.
    let g_count = graphite_scalar(&c, "SELECT count(*) FROM f;");
    let s_count = Command::new("sqlite3")
        .arg(&path)
        .arg("SELECT count(*) FROM f;")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap();
    assert_eq!(g_count, s_count, "row count mismatch for {tag}");
    assert_eq!(
        g_count,
        (ins - del).to_string(),
        "wrong surviving count for {tag}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn delete_heavy_fts5_files_are_sqlite_valid() {
    if !have_fts5_sqlite() {
        eprintln!("skipping: sqlite3 with FTS5 not on PATH");
        return;
    }
    // The exact reported repro plus several other delete-heavy shapes.
    assert_delete_heavy_valid("150-100", 150, 100);
    assert_delete_heavy_valid("175-120", 175, 120);
    assert_delete_heavy_valid("200-150", 200, 150);
    assert_delete_heavy_valid("137-133", 137, 133);
    assert_delete_heavy_valid("250-249", 250, 249);
    assert_delete_heavy_valid("90-89", 90, 89);
}
