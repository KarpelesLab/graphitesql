//! Every table-valued function exposes an implicit `rowid` column, as SQLite does.
//! The value is TVF-specific: `json_each`/`json_tree` use a 0-based counter over the
//! emitted rows, `generate_series` uses the value itself, and a `pragma_*` TVF uses
//! the 1-based row number. `rowid` is a hidden column — selectable and usable in
//! `WHERE`/`ORDER BY`, but excluded from `*` expansion.
//!
//! graphite previously exposed no `rowid` on any TVF, so `SELECT rowid FROM
//! json_each(…)` failed with `no such column: rowid`. Verified vs sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s.trim_end().to_string()
}

#[test]
fn table_valued_functions_expose_rowid() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // json_each / json_tree: 0-based row counter.
        "SELECT rowid, key, value FROM json_each('[10,20,30]')",
        "SELECT rowid, key FROM json_each('{\"a\":1,\"b\":2}')",
        "SELECT rowid, fullkey FROM json_tree('{\"a\":[1,2]}')",
        "SELECT value FROM json_each('[10,20,30]') WHERE rowid=1",
        "SELECT value FROM json_each('[30,10,20]') ORDER BY rowid DESC",
        "SELECT max(rowid), count(*) FROM json_each('[1,2,3,4,5]')",
        // rowid is excluded from `*`.
        "SELECT * FROM json_each('[1,2]')",
        // generate_series: rowid == value.
        "SELECT rowid, value FROM generate_series(5,20,5)",
        "SELECT value FROM generate_series(10,20,5) WHERE rowid=15",
        "SELECT * FROM generate_series(1,3)",
        // pragma TVFs: 1-based row number.
        "CREATE TABLE t(a,b,c); SELECT rowid, cid, name FROM pragma_table_info('t')",
        "CREATE TABLE t(a UNIQUE,b UNIQUE); SELECT rowid, name FROM pragma_index_list('t')",
        "CREATE TABLE t(a,b); SELECT * FROM pragma_table_info('t')",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
