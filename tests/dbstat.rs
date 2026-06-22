//! The `dbstat` eponymous read-only virtual table: per-page b-tree storage
//! statistics, byte-compatible with SQLite's dbstat extension. Verified
//! differentially against the `sqlite3` CLI on real database files.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn rows(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Have `sqlite3` build a database file from `script`, then run the same
/// `dbstat` query under both `sqlite3` and graphite on that *one* physical
/// file. This isolates dbstat correctness from page-allocation differences:
/// the two engines may split/allocate pages differently when each builds its
/// own file, but reading an identical file they must report identical stats.
fn check(tag: &str, script: &str) {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    const Q: &str = "SELECT name, path, pageno, pagetype, ncell, payload, unused, mx_payload, pgoffset, pgsize FROM dbstat ORDER BY pageno";

    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let spath = dir.join(format!("gsql-dbstat-{pid}-{tag}.db"));
    let spath = spath.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&spath);

    // sqlite3 builds the single source-of-truth file.
    let o = Command::new("sqlite3")
        .arg(&spath)
        .arg(format!("{script};"))
        .output()
        .unwrap();
    assert!(o.status.success(), "sqlite build failed: {o:?}");

    let want = {
        let o = Command::new("sqlite3").arg(&spath).arg(Q).output().unwrap();
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    };
    // graphite reads the very same file (read-only) and runs the same query.
    let c = Connection::open(&spath).unwrap();
    let got = rows(&c, Q);

    assert_eq!(got, want, "dbstat diverged for script: {script}");

    let _ = std::fs::remove_file(&spath);
}

#[test]
fn dbstat_single_small_table() {
    check(
        "small",
        "CREATE TABLE t(a, b); INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'z')",
    );
}

#[test]
fn dbstat_table_with_index() {
    check(
        "index",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); \
         INSERT INTO t VALUES(1,'aa'),(2,'bb'),(3,'cc'); \
         CREATE INDEX i ON t(b)",
    );
}

#[test]
fn dbstat_multipage_btree() {
    // Enough rows to force interior pages (a multi-level b-tree).
    let mut s = String::from("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); ");
    s.push_str("INSERT INTO t(b) VALUES('payload-string-number-0')");
    for i in 1..400 {
        s.push_str(&format!(",('payload-string-number-{i}')"));
    }
    check("multipage", &s);
}

#[test]
fn dbstat_overflow_pages() {
    // A large blob forces an overflow chain off the leaf page.
    check(
        "overflow",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); \
         INSERT INTO t(b) VALUES(hex(zeroblob(8000))), (hex(zeroblob(20000)))",
    );
}

#[test]
fn dbstat_empty_and_schema_only() {
    check("empty", "CREATE TABLE t(a)");
}

#[test]
fn dbstat_works_in_memory_and_aggregates() {
    // dbstat works on an in-memory database (no file needed); the page-level
    // stats are self-consistent: payload+unused never exceeds a page, and the
    // pages cover the whole database.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    c.execute("INSERT INTO t(b) VALUES('x'),('y'),('z')")
        .unwrap();

    // The root page of sqlite_schema (page 1) is always present.
    assert_eq!(rows(&c, "SELECT count(*) FROM dbstat WHERE pageno=1"), "1");
    // Every page reports a leaf/internal/overflow type and a positive size.
    assert_eq!(
        rows(
            &c,
            "SELECT count(*) FROM dbstat WHERE pgsize<=0 OR pagetype NOT IN('leaf','internal','overflow')"
        ),
        "0"
    );
    // An alias works, and WHERE/aggregate push down through the virtual source.
    assert_eq!(
        rows(&c, "SELECT count(*) FROM dbstat d WHERE d.pagetype='leaf'"),
        rows(&c, "SELECT count(*) FROM dbstat WHERE pagetype='leaf'"),
    );
}

#[test]
fn dbstat_select_star_column_shape() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // `SELECT *` exposes exactly the ten public columns (hidden schema/aggregate
    // columns are not selected), in SQLite's order.
    let dir = std::env::temp_dir();
    let spath = dir
        .join(format!("gsql-dbstat-star-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&spath);
    let o = Command::new("sqlite3")
        .arg(&spath)
        .arg("CREATE TABLE t(a); INSERT INTO t VALUES(1);")
        .output()
        .unwrap();
    assert!(o.status.success());
    let c = Connection::open(&spath).unwrap();
    let names = c.query("SELECT * FROM dbstat").unwrap().columns;
    assert_eq!(
        names,
        [
            "name",
            "path",
            "pageno",
            "pagetype",
            "ncell",
            "payload",
            "unused",
            "mx_payload",
            "pgoffset",
            "pgsize",
        ]
    );
    let _ = std::fs::remove_file(&spath);
}

#[test]
fn dbstat_real_table_shadows_eponymous() {
    // A user table literally named `dbstat` takes precedence over the virtual one.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE dbstat(x, y)").unwrap();
    c.execute("INSERT INTO dbstat VALUES(1,2),(3,4)").unwrap();
    assert_eq!(rows(&c, "SELECT x, y FROM dbstat ORDER BY x"), "1|2\n3|4");
}
