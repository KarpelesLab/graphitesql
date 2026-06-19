//! End-to-end tests against real databases produced by the `sqlite3` CLI.
//!
//! These open the committed fixtures in `tests/fixtures/` through the std-file
//! VFS and the pager, proving graphitesql reads genuine SQLite files. As later
//! phases land (b-tree, records, SQL), this file grows to assert on actual row
//! contents.

#![cfg(feature = "std")]

use graphitesql::format::TextEncoding;
use graphitesql::pager::Pager;
use graphitesql::vfs::{std_file::StdVfs, OpenFlags, Vfs};

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
