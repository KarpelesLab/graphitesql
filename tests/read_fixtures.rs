//! End-to-end tests against real databases produced by the `sqlite3` CLI.
//!
//! These open the committed fixtures in `tests/fixtures/` through the std-file
//! VFS and the pager, proving graphitesql reads genuine SQLite files. As later
//! phases land (b-tree, records, SQL), this file grows to assert on actual row
//! contents.

#![cfg(feature = "std")]

use graphitesql::btree::{IndexCursor, TableCursor};
use graphitesql::format::{decode_record, TextEncoding};
use graphitesql::pager::Pager;
use graphitesql::schema::{ObjectType, Schema};
use graphitesql::vfs::{std_file::StdVfs, OpenFlags, Vfs};
use graphitesql::Value;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn open_pager(name: &str) -> Pager {
    let vfs = StdVfs::new();
    let file = vfs
        .open(&fixture(name), OpenFlags::READ_ONLY)
        .expect("open fixture");
    Pager::open(file).expect("open pager")
}

#[test]
fn reads_basic_db_header_and_pages() {
    let pager = open_pager("basic.db");
    let h = pager.header();
    assert_eq!(h.page_size, 4096);
    assert_eq!(h.text_encoding, TextEncoding::Utf8);
    assert_eq!(pager.page_count(), 3); // confirmed via `PRAGMA page_count`
    assert!(h.size_in_pages_valid());
    assert_eq!(h.size_in_pages, 3);

    // Page 1 carries the header; its body (b-tree) starts at offset 100.
    let p1 = pager.page(1).unwrap();
    assert_eq!(p1.body_offset(), 100);
    // Every page must be readable.
    for n in 1..=pager.page_count() {
        let p = pager.page(n).unwrap();
        assert_eq!(p.data().len(), pager.page_size());
    }
}

#[test]
fn reads_big_db_with_many_pages() {
    let pager = open_pager("big.db");
    assert_eq!(pager.page_size(), 4096);
    assert_eq!(pager.page_count(), 15); // confirmed via `PRAGMA page_count`
                                        // Reading the last page must succeed; reading past it must fail.
    assert!(pager.page(15).is_ok());
    assert!(pager.page(16).is_err());
}

#[test]
fn table_scan_basic_t() {
    // basic.db: table `t` root page 2, rows with rowid 1,2,3.
    let pager = open_pager("basic.db");
    let mut cur = TableCursor::new(&pager, 2);
    let mut rowids = Vec::new();
    let mut ok = cur.first().unwrap();
    while ok {
        rowids.push(cur.rowid().unwrap());
        assert!(!cur.payload().unwrap().is_empty());
        ok = cur.next().unwrap();
    }
    assert_eq!(rowids, vec![1, 2, 3]);
}

#[test]
fn table_seek_basic_t() {
    let pager = open_pager("basic.db");
    let mut cur = TableCursor::new(&pager, 2);
    assert!(cur.seek(2).unwrap()); // exact hit
    assert_eq!(cur.rowid().unwrap(), 2);
    assert!(!cur.seek(99).unwrap()); // past the end
    assert!(!cur.is_valid());
    assert!(!cur.seek(0).unwrap()); // before the first -> lands on rowid 1
    assert_eq!(cur.rowid().unwrap(), 1);
}

#[test]
fn table_scan_big_nums_interior_pages() {
    // big.db: table `nums` root page 2, 2000 rows -> requires interior pages.
    let pager = open_pager("big.db");
    let mut cur = TableCursor::new(&pager, 2);
    let mut count = 0i64;
    let mut sum = 0i64;
    let mut prev = 0i64;
    let mut ok = cur.first().unwrap();
    while ok {
        let rid = cur.rowid().unwrap();
        assert!(rid > prev, "rowids must be strictly ascending");
        prev = rid;
        sum += rid;
        count += 1;
        ok = cur.next().unwrap();
    }
    assert_eq!(count, 2000);
    assert_eq!(sum, 2_001_000); // confirmed via SELECT sum(id)
}

#[test]
fn table_scan_big_blob_overflow() {
    // big.db: table `big` root page 11, one row with a 20000-byte blob payload
    // that must span overflow pages.
    let pager = open_pager("big.db");
    let mut cur = TableCursor::new(&pager, 11);
    assert!(cur.first().unwrap());
    let payload = cur.payload().unwrap();
    // Record = header + a 20000-byte blob body, so reassembled payload must be
    // well beyond a single 4096-byte page: proves overflow reassembly works.
    assert!(payload.len() > 20000, "got {} bytes", payload.len());
    assert!(!cur.next().unwrap());
}

#[test]
fn index_scan_basic_idx_b() {
    // basic.db: index `idx_b` root page 3, one entry per row in `t` (3 rows).
    let pager = open_pager("basic.db");
    let mut cur = IndexCursor::new(&pager, 3);
    let mut count = 0;
    while let Some(payload) = cur.next().unwrap() {
        assert!(!payload.is_empty());
        count += 1;
    }
    assert_eq!(count, 3);
}

#[test]
fn reads_schema_catalog() {
    let pager = open_pager("basic.db");
    let schema = Schema::read(&pager).unwrap();

    let t = schema.table("t").expect("table t in catalog");
    assert_eq!(t.obj_type, ObjectType::Table);
    assert_eq!(t.rootpage, 2); // matches SELECT rootpage FROM sqlite_schema
    assert!(t.sql.as_deref().unwrap().contains("CREATE TABLE"));

    let idx = schema.index("idx_b").expect("index idx_b in catalog");
    assert_eq!(idx.obj_type, ObjectType::Index);
    assert_eq!(idx.tbl_name, "t");
    assert_eq!(idx.rootpage, 3);

    // Exactly one index attached to `t`.
    assert_eq!(schema.indexes_on("t").count(), 1);
}

#[test]
fn schema_drives_table_scan_end_to_end() {
    // Resolve a table name through the catalog, then decode its rows — the full
    // read path from a name to typed values.
    let pager = open_pager("basic.db");
    let schema = Schema::read(&pager).unwrap();
    let root = schema.table("t").unwrap().rootpage;

    let mut cur = TableCursor::new(&pager, root);
    let mut decoded_rows = Vec::new();
    let mut ok = cur.first().unwrap();
    while ok {
        let rowid = cur.rowid().unwrap();
        let cols = decode_record(&cur.payload().unwrap(), TextEncoding::Utf8).unwrap();
        decoded_rows.push((rowid, cols));
        ok = cur.next().unwrap();
    }

    assert_eq!(decoded_rows.len(), 3);
    // Row 1: (1,'hello',3.14, x'01020304'). Column `a` is INTEGER PRIMARY KEY,
    // so it is stored as NULL in the record and aliases the rowid.
    let (rowid, cols) = &decoded_rows[0];
    assert_eq!(*rowid, 1);
    assert_eq!(cols[0], Value::Null); // IPK alias -> stored NULL
    assert_eq!(cols[1], Value::Text("hello".into()));
    #[allow(clippy::approx_constant)] // 3.14 is fixture data, not π
    let pi_ish = 3.14;
    assert_eq!(cols[2], Value::Real(pi_ish));
    assert_eq!(cols[3], Value::Blob(vec![1, 2, 3, 4]));
    // Row 3 has NULLs for b and c.
    let (_, cols3) = &decoded_rows[2];
    assert_eq!(cols3[1], Value::Null);
    assert_eq!(cols3[2], Value::Null);
    assert_eq!(cols3[3], Value::Blob(vec![0xff]));
}

#[test]
fn rejects_non_database_file() {
    let vfs = StdVfs::new();
    // The Cargo.toml is definitely not a SQLite file.
    let file = vfs
        .open(
            &format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR")),
            OpenFlags::READ_ONLY,
        )
        .unwrap();
    assert!(Pager::open(file).is_err());
}
