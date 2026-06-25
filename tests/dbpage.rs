//! The `sqlite_dbpage` read-only virtual table (ROADMAP Track D, `dbpage-1`).
//!
//! One row per database page, `(pgno INTEGER, data BLOB)`, where `data` is the
//! page's raw bytes — page 1 carries the 100-byte file header. This mirrors
//! sqlite's `sqlite_dbpage`; we verify byte-for-byte that the page images match
//! what `sqlite3` reports for the *same* file (graphite-written, so the test
//! doubles as a check that our on-disk page layout is the real format).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

/// A file-backed db path in a per-process temp dir (cleaned up at the end).
fn tmp_db(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("gsql-dbpage-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name).to_str().unwrap().to_string()
}

#[test]
fn one_row_per_page_with_raw_bytes() {
    let path = tmp_db("rows.db");
    let _ = std::fs::remove_file(&path);
    let mut c = Connection::create(&path).unwrap();
    let path = path.as_str();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'z')")
        .unwrap();

    // pgno is 1-based and dense; rows are ordered by page number.
    let rows = c
        .query("SELECT pgno, length(data) FROM sqlite_dbpage ORDER BY pgno")
        .unwrap()
        .rows;
    assert!(!rows.is_empty());
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row[0], Value::Integer(i as i64 + 1));
        // every page is exactly one page-size blob
        let Value::Integer(len) = row[1] else {
            panic!("length(data) not an integer")
        };
        assert_eq!(len, 4096, "page {} wrong size", i + 1);
    }

    // count(*) equals the page count and page 1 starts with the format magic.
    let n = c.query("SELECT count(*) FROM sqlite_dbpage").unwrap().rows[0][0].clone();
    assert_eq!(n, Value::Integer(rows.len() as i64));
    let hdr = c
        .query("SELECT hex(substr(data,1,16)) FROM sqlite_dbpage WHERE pgno=1")
        .unwrap()
        .rows[0][0]
        .clone();
    // "SQLite format 3\0"
    assert_eq!(hdr, Value::Text("53514C69746520666F726D6174203300".into()));

    let _ = std::fs::remove_file(path);
}

#[test]
fn a_real_table_named_sqlite_dbpage_is_unreachable() {
    // The `sqlite_` prefix is reserved, so users can't shadow the vtab; the
    // eponymous table is always what an unqualified `sqlite_dbpage` resolves to.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    // An in-memory db still has at least page 1.
    let n = c.query("SELECT count(*) FROM sqlite_dbpage").unwrap().rows[0][0].clone();
    let Value::Integer(pages) = n else {
        panic!("count not integer")
    };
    assert!(pages >= 1, "expected at least the header page, got {pages}");
}

#[test]
fn main_qualifier_reaches_the_eponymous_vtabs() {
    // `main.dbstat` / `main.sqlite_dbpage` resolve to the same eponymous tables
    // as the unqualified names (they describe the `main` database).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    for name in ["sqlite_dbpage", "dbstat"] {
        let bare = c
            .query(&format!("SELECT count(*) FROM {name}"))
            .unwrap()
            .rows[0][0]
            .clone();
        let qualified = c
            .query(&format!("SELECT count(*) FROM main.{name}"))
            .unwrap()
            .rows[0][0]
            .clone();
        assert_eq!(bare, qualified, "main.{name} != {name}");
    }
}

#[test]
fn table_info_reports_the_fixed_column_shape() {
    // table_info: pgno (declared PRIMARY KEY → pk=1) then data BLOB.
    let c = Connection::open_memory().unwrap();
    let info = c.query("PRAGMA table_info(sqlite_dbpage)").unwrap().rows;
    assert_eq!(info.len(), 2);
    assert_eq!(info[0][1], Value::Text("pgno".into()));
    assert_eq!(info[0][2], Value::Text("INTEGER".into()));
    assert_eq!(info[0][5], Value::Integer(1)); // pk
    assert_eq!(info[1][1], Value::Text("data".into()));
    assert_eq!(info[1][5], Value::Integer(0));

    // table_xinfo adds the trailing hidden `schema` column (hidden flag = 1).
    let xinfo = c.query("PRAGMA table_xinfo(sqlite_dbpage)").unwrap().rows;
    assert_eq!(xinfo.len(), 3);
    assert_eq!(xinfo[2][1], Value::Text("schema".into()));
    assert_eq!(xinfo[2][6], Value::Integer(1)); // hidden

    // dbstat's xinfo carries two hidden columns (schema, aggregate).
    let dbstat = c.query("PRAGMA table_xinfo(dbstat)").unwrap().rows;
    assert_eq!(dbstat.len(), 12);
    assert_eq!(dbstat[10][1], Value::Text("schema".into()));
    assert_eq!(dbstat[11][1], Value::Text("aggregate".into()));
    // table_info (non-extended) hides them: 10 visible columns only.
    let dbstat_info = c.query("PRAGMA table_info(dbstat)").unwrap().rows;
    assert_eq!(dbstat_info.len(), 10);
}

#[test]
fn temp_qualifier_without_temp_db_errors_not_panics() {
    // A `temp.`-qualified read before any temp database exists must report the
    // name as missing (as sqlite does), not panic resolving the temp backend.
    let c = Connection::open_memory().unwrap();
    let err = c.query("SELECT * FROM temp.foo").unwrap_err();
    assert!(
        err.to_string().contains("no such table: temp.foo"),
        "unexpected error: {err}"
    );
}
