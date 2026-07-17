//! Whole-file page accounting in `PRAGMA integrity_check` — the port of
//! sqlite's `IntegrityCk.aPgRef` bitmap protocol (`sqlite3BtreeIntegrityCheck`
//! / `checkTreePage` / `checkList` / `checkRef`).
//!
//! Before this layer, graphite's integrity_check walked each b-tree with its
//! own per-tree visited set, so cross-tree damage self-reported `ok` while real
//! sqlite3 said `database disk image is malformed`: a page referenced by two
//! trees, a live b-tree page also on the freelist (the shape a stale freelist
//! write leaves behind — a real production corruption), orphaned (leaked)
//! pages, and a freelist whose traversal disagrees with the header count.
//!
//! Each test builds a valid database with graphite, surgically corrupts it with
//! deterministic byte patches, and asserts graphite now reports the problem
//! with sqlite-comparable messages (and that sqlite3 3.50.4, when present,
//! agrees the file is damaged). Clean databases of every flavor must keep
//! reporting exactly `ok`.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn load(bin: &str, db: &str, sql: &str) {
    let _ = std::fs::remove_file(db);
    let out = Command::new(bin).arg(db).arg(sql).output().expect("run");
    assert!(
        out.status.success(),
        "load failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn query(bin: &str, db: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(db).arg(sql).output().expect("query");
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

fn tmp(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphite_ipa_{}_{}.db", std::process::id(), name));
    p.to_string_lossy().into_owned()
}

fn be_u32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

/// Build a graphite database that has a table `t` (root page 3, after `a` is
/// dropped), an index, overflow chains, and a non-empty freelist (from the
/// `DROP TABLE`). Returns its path.
fn build_base(name: &str) -> String {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let db = tmp(name);
    load(
        g,
        &db,
        "CREATE TABLE a(x);
         INSERT INTO a SELECT printf('%.*c',500,'x') FROM generate_series(1,200);
         CREATE TABLE t(id INTEGER PRIMARY KEY, k INT, v TEXT);
         CREATE INDEX ik ON t(k);
         INSERT INTO t(k,v) SELECT value, printf('%.*c',300,'y') FROM generate_series(1,100);
         DROP TABLE a;",
    );
    assert_eq!(query(g, &db, "PRAGMA integrity_check"), "ok", "base not ok");
    assert_ne!(
        query(g, &db, "PRAGMA freelist_count"),
        "0",
        "base has no freelist; the corruption patches need one"
    );
    db
}

/// The header's freelist head: (first trunk page, freelist page count).
fn freelist_head(db: &str) -> (u32, u32) {
    let d = std::fs::read(db).unwrap();
    (be_u32(&d, 32), be_u32(&d, 36))
}

/// The motivating production shape: a stale freelist entry pointing at a LIVE
/// b-tree page. The page is reachable twice — once via its tree, once via the
/// freelist — so a later allocation would clobber live data. sqlite reports
/// `2nd reference to page N`; graphite formerly said `ok`.
#[test]
fn live_page_on_freelist_is_second_reference() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let db = build_base("dualref");
    let (trunk, _) = freelist_head(&db);
    let root: u32 = query(g, &db, "SELECT rootpage FROM sqlite_schema WHERE name='t'")
        .parse()
        .unwrap();

    // Patch the trunk's first leaf slot to point at t's root page.
    let mut d = std::fs::read(&db).unwrap();
    let page_size = u16::from_be_bytes([d[16], d[17]]) as usize;
    let off = (trunk as usize - 1) * page_size;
    let n_leaves = be_u32(&d, off + 4);
    assert!(n_leaves > 0, "trunk page carries no leaves");
    d[off + 8..off + 12].copy_from_slice(&root.to_be_bytes());
    std::fs::write(&db, &d).unwrap();

    let report = query(g, &db, "PRAGMA integrity_check");
    assert!(
        report.contains(&format!("2nd reference to page {root}")),
        "graphite must flag the doubly-referenced page, got:\n{report}"
    );
    if sqlite3_available() {
        let s = query("sqlite3", &db, "PRAGMA integrity_check");
        assert!(
            s.contains(&format!("2nd reference to page {root}")),
            "sqlite3 disagrees on the same file:\n{s}"
        );
    }
    let _ = std::fs::remove_file(&db);
}

/// Leaked pages: wiping the freelist header (trunk = 0, count = 0) makes every
/// formerly-free page unreachable from any tree. sqlite reports each as
/// `Page N: never used`; graphite formerly said `ok`.
#[test]
fn orphaned_pages_are_never_used() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let db = build_base("orphan");
    let mut d = std::fs::read(&db).unwrap();
    d[32..40].fill(0);
    std::fs::write(&db, &d).unwrap();

    let report = query(g, &db, "PRAGMA integrity_check");
    assert!(
        report.contains(": never used"),
        "graphite must flag the leaked pages, got:\n{report}"
    );
    if sqlite3_available() {
        let s = query("sqlite3", &db, "PRAGMA integrity_check");
        // Same set of `Page N: never used` lines, in the same order.
        let expect: Vec<&str> = s.lines().filter(|l| l.ends_with("never used")).collect();
        let got: Vec<&str> = report
            .lines()
            .filter(|l| l.ends_with("never used"))
            .collect();
        assert_eq!(got, expect, "orphan reports diverge from sqlite3");
    }
    let _ = std::fs::remove_file(&db);
}

/// A freelist whose traversal disagrees with the header's count reports
/// sqlite's `Freelist: size is X but should be Y`.
#[test]
fn freelist_count_mismatch() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let db = build_base("flcount");
    let (_, count) = freelist_head(&db);
    let mut d = std::fs::read(&db).unwrap();
    d[36..40].copy_from_slice(&(count + 3).to_be_bytes());
    std::fs::write(&db, &d).unwrap();

    let expect = format!("Freelist: size is {count} but should be {}", count + 3);
    let report = query(g, &db, "PRAGMA integrity_check");
    assert!(
        report.contains(&expect),
        "expected `{expect}`, got:\n{report}"
    );
    if sqlite3_available() {
        let s = query("sqlite3", &db, "PRAGMA integrity_check");
        assert!(s.contains(&expect), "sqlite3 message differs:\n{s}");
    }
    let _ = std::fs::remove_file(&db);
}

/// An out-of-range freelist entry reports sqlite's
/// `Freelist: invalid page number N` (and the displaced page leaks).
#[test]
fn freelist_out_of_range_entry() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let db = build_base("flrange");
    let (trunk, _) = freelist_head(&db);
    let mut d = std::fs::read(&db).unwrap();
    let page_size = u16::from_be_bytes([d[16], d[17]]) as usize;
    let off = (trunk as usize - 1) * page_size;
    d[off + 8..off + 12].copy_from_slice(&50000u32.to_be_bytes());
    std::fs::write(&db, &d).unwrap();

    let report = query(g, &db, "PRAGMA integrity_check");
    assert!(
        report.contains("Freelist: invalid page number 50000"),
        "expected invalid-page report, got:\n{report}"
    );
    if sqlite3_available() {
        let s = query("sqlite3", &db, "PRAGMA integrity_check");
        assert!(
            s.contains("Freelist: invalid page number 50000"),
            "sqlite3 message differs:\n{s}"
        );
    }
    let _ = std::fs::remove_file(&db);
}

/// `PRAGMA integrity_check(N)` caps the report at N rows, like sqlite's
/// `SQLITE_INTEGRITY_CHECK_ERROR_MAX` machinery.
#[test]
fn error_limit_argument() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let db = build_base("cap");
    let mut d = std::fs::read(&db).unwrap();
    d[32..40].fill(0); // orphan every freelist page (many problems)
    std::fs::write(&db, &d).unwrap();

    let full = query(g, &db, "PRAGMA integrity_check");
    assert!(full.lines().count() > 1, "need several problems: {full}");
    let capped = query(g, &db, "PRAGMA integrity_check(1)");
    assert_eq!(capped.lines().count(), 1, "cap not honored: {capped}");
    assert_eq!(capped, full.lines().next().unwrap());
    if sqlite3_available() {
        // sqlite returns its page-level report as ONE row (newline-separated);
        // with a cap of 1 it, too, keeps only the first message.
        let s = query("sqlite3", &db, "PRAGMA integrity_check(1)");
        let s_msgs: Vec<&str> = s
            .lines()
            .filter(|l| !l.starts_with("*** in database"))
            .collect();
        assert_eq!(s_msgs.len(), 1, "sqlite3 cap behavior changed: {s}");
        assert_eq!(s_msgs[0], capped);
    }
    let _ = std::fs::remove_file(&db);
}

/// No false positives: clean graphite-built databases of every storage flavor
/// (rowid + indexes + overflow, freelist from a DROP, WITHOUT ROWID,
/// AUTOINCREMENT's sqlite_sequence, auto_vacuum FULL) keep reporting exactly
/// `ok`, and sqlite3 agrees with each file.
#[test]
fn clean_flavors_still_ok() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let flavors: &[(&str, &str)] = &[
        (
            "rowid",
            "CREATE TABLE t(id INTEGER PRIMARY KEY, k INT, v TEXT);
             CREATE INDEX ik ON t(k);
             INSERT INTO t(k,v) SELECT value, printf('%.*c',300,'x') FROM generate_series(1,300);
             INSERT INTO t(k,v) VALUES (9, printf('%.*c',20000,'y'));
             DELETE FROM t WHERE id%3=0;",
        ),
        (
            "freelist",
            "CREATE TABLE a(x); CREATE TABLE b(y);
             INSERT INTO a SELECT printf('%.*c',500,'x') FROM generate_series(1,200);
             INSERT INTO b SELECT printf('%.*c',500,'y') FROM generate_series(1,100);
             DROP TABLE a;",
        ),
        (
            "without_rowid",
            "CREATE TABLE w(k TEXT PRIMARY KEY, v) WITHOUT ROWID;
             INSERT INTO w SELECT printf('key%d',value), printf('%.*c',1500,'q')
               FROM generate_series(1,150);
             CREATE INDEX wv ON w(v);",
        ),
        (
            "autoincrement",
            "CREATE TABLE s(id INTEGER PRIMARY KEY AUTOINCREMENT, x);
             INSERT INTO s(x) VALUES (1),(2),(3);",
        ),
        (
            "auto_vacuum",
            "PRAGMA auto_vacuum=FULL;
             CREATE TABLE t(a,b); CREATE INDEX i1 ON t(a);
             INSERT INTO t SELECT value, printf('%.*c',1000,'z') FROM generate_series(1,400);
             DELETE FROM t WHERE a%2=0;",
        ),
    ];
    for (name, sql) in flavors {
        let db = tmp(&format!("clean_{name}"));
        load(g, &db, sql);
        assert_eq!(
            query(g, &db, "PRAGMA integrity_check"),
            "ok",
            "[{name}] graphite flags a clean database"
        );
        if sqlite3_available() {
            assert_eq!(
                query("sqlite3", &db, "PRAGMA integrity_check"),
                "ok",
                "[{name}] sqlite3 rejects the graphite-built file"
            );
        }
        let _ = std::fs::remove_file(&db);
    }
}
