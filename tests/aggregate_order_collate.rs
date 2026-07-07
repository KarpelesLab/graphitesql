//! An `ORDER BY` inside an ordered aggregate (`group_concat(x ORDER BY y …)` /
//! `string_agg`) sorts the collected values under that term's collation — an
//! explicit `COLLATE`, else the key column's own collation, else BINARY. graphite's
//! VDBE aggregate sorter (`AggOrderKey` / `cmp_key_rows`) hard-coded BINARY, so an
//! explicit `COLLATE` on the aggregate's `ORDER BY` was ignored: `group_concat(v
//! ORDER BY v COLLATE NOCASE)` came out in binary order (`A,B,C,a`) instead of
//! sqlite's `a,A,B,C`. Verified byte-for-byte against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn rows(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn aggregate_order_by_collate_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let t = "CREATE TABLE t(v TEXT);INSERT INTO t VALUES('B'),('a'),('C'),('A');";
    let w = "CREATE TABLE w(a TEXT, b TEXT);INSERT INTO w VALUES('1','B'),('2','a'),('3','C');";
    let m = "CREATE TABLE m(a TEXT, b INT);INSERT INTO m VALUES('B',1),('a',2),('a',1),('C',3);";
    let ucol = "CREATE TABLE u(v TEXT COLLATE NOCASE);INSERT INTO u VALUES('B'),('a'),('C'),('A');";
    let n = "CREATE TABLE n(x INT);INSERT INTO n VALUES(3),(1),(2);";
    let cases: &[String] = &[
        // the fix: explicit COLLATE on the aggregate's ORDER BY term
        format!("{t}SELECT group_concat(v ORDER BY v COLLATE NOCASE) FROM t;"),
        format!("{t}SELECT group_concat(v ORDER BY v COLLATE NOCASE DESC) FROM t;"),
        format!("{t}SELECT string_agg(v,',' ORDER BY v COLLATE NOCASE) FROM t;"),
        format!("{w}SELECT group_concat(a ORDER BY b COLLATE NOCASE) FROM w;"),
        format!("{m}SELECT group_concat(a||b ORDER BY a COLLATE NOCASE, b DESC) FROM m;"),
        // grouped
        format!(
            "{t}SELECT group_concat(v ORDER BY v COLLATE NOCASE) FROM t \
             GROUP BY v>'M' COLLATE NOCASE;"
        ),
        // must NOT regress: BINARY default, a column-defined collation, numeric,
        // DESC, and ordering by a different (unmentioned) column
        format!("{t}SELECT group_concat(v ORDER BY v) FROM t;"),
        format!("{ucol}SELECT group_concat(v ORDER BY v) FROM u;"),
        format!("{n}SELECT group_concat(x ORDER BY x DESC) FROM n;"),
        format!("{t}SELECT group_concat(v ORDER BY v DESC) FROM t;"),
        "CREATE TABLE t2(v,k);INSERT INTO t2 VALUES('a',3),('b',1),('c',2);\
         SELECT group_concat(v ORDER BY k) FROM t2;"
            .to_string(),
    ];
    for q in cases {
        assert_eq!(rows("sqlite3", q), rows(g, q), "mismatch for `{q}`");
    }
}
