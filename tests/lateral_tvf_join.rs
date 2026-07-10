//! Lateral / correlated table-valued functions: `FROM t, json_each(t.data)` — a TVF
//! whose argument references an outer FROM column, re-evaluated per outer row (the
//! common "expand a JSON column per row" pattern). graphite previously errored
//! `no such column: t.data` because the TVF was materialized once in a rowless
//! context; it now re-materializes the function for each outer row with that row's
//! columns bound, cross-joining the results.
//!
//! Verified against sqlite3 3.50.4.

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
fn lateral_correlated_table_valued_functions_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Comma join, qualified and unqualified argument.
        "CREATE TABLE t(id, data); INSERT INTO t VALUES(1,'[10,20]'),(2,'[30]'); \
         SELECT t.id, j.value FROM t, json_each(t.data) j ORDER BY t.id, j.value",
        "CREATE TABLE t(data); INSERT INTO t VALUES('[1,2]'); \
         SELECT value FROM t, json_each(data) ORDER BY value",
        // Explicit JOIN, and JOIN … ON.
        "CREATE TABLE t(data); INSERT INTO t VALUES('[1,2]'),('[3]'); \
         SELECT value FROM t JOIN json_each(t.data) ORDER BY value",
        "CREATE TABLE t(id,data); INSERT INTO t VALUES(1,'[5,15,25]'); \
         SELECT value FROM t JOIN json_each(t.data) ON value>10 ORDER BY value",
        // Object keys, json_tree, and a computed (json_extract) argument.
        "CREATE TABLE t(id,j); INSERT INTO t VALUES(1,'{\"x\":1,\"y\":2}'),(2,'{\"z\":3}'); \
         SELECT t.id, key, value FROM t, json_each(t.j) ORDER BY t.id, key",
        "CREATE TABLE t(j); INSERT INTO t VALUES('{\"x\":[1,2]}'); \
         SELECT fullkey, atom FROM t, json_tree(t.j) WHERE atom IS NOT NULL ORDER BY fullkey",
        "CREATE TABLE t(j); INSERT INTO t VALUES('{\"items\":[7,8]}'); \
         SELECT value FROM t, json_each(json_extract(t.j,'$.items')) ORDER BY value",
        // generate_series with correlated bounds.
        "CREATE TABLE t(lo,hi); INSERT INTO t VALUES(1,3),(5,6); \
         SELECT t.lo, s.value FROM t, generate_series(t.lo,t.hi) s ORDER BY t.lo, s.value",
        // LEFT JOIN null-pads an outer row with an empty expansion.
        "CREATE TABLE t(id,data); INSERT INTO t VALUES(1,'[10]'),(2,'[]'); \
         SELECT t.id, j.value FROM t LEFT JOIN json_each(t.data) j ORDER BY t.id",
        // NULL / empty document yields no rows for that outer row.
        "CREATE TABLE t(id,data); INSERT INTO t VALUES(1,'[10]'),(2,NULL),(3,'[30]'); \
         SELECT t.id, j.value FROM t, json_each(t.data) j ORDER BY t.id",
        // Aggregation grouped by the outer row.
        "CREATE TABLE t(id,data); INSERT INTO t VALUES(1,'[1,2,3]'),(2,'[10]'); \
         SELECT t.id, sum(j.value) FROM t, json_each(t.data) j GROUP BY t.id",
        // Two chained lateral TVFs.
        "CREATE TABLE t(d1,d2); INSERT INTO t VALUES('[1,2]','[10,20]'); \
         SELECT a.value, b.value FROM t, json_each(t.d1) a, json_each(t.d2) b \
         ORDER BY a.value, b.value",
        // A third ordinary table after the lateral TVF.
        "CREATE TABLE t(a,data); INSERT INTO t VALUES(1,'[10]'); CREATE TABLE u(b); \
         INSERT INTO u VALUES(100); SELECT a,value,b FROM t, json_each(t.data), u",
        // The TVF's rowid is accessible via its alias, and `*` excludes the hidden
        // columns (rowid / json / root).
        "CREATE TABLE t(data); INSERT INTO t VALUES('[10,20]'); \
         SELECT j.rowid, j.value FROM t, json_each(t.data) j",
        "CREATE TABLE t(x,data); INSERT INTO t VALUES(1,'[5]'); SELECT * FROM t, json_each(t.data)",
        // Empty outer table: no rows, resolves the inner columns.
        "CREATE TABLE t(data); SELECT j.value FROM t, json_each(t.data) j",
        // A non-correlated TVF in a join is unaffected.
        "CREATE TABLE t(id); INSERT INTO t VALUES(1),(2); \
         SELECT t.id, s.value FROM t, generate_series(1,2) s ORDER BY t.id, s.value",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
