//! A forward inner-seek in a join whose index holds every column of the inner
//! table the query needs renders `SEARCH … USING COVERING INDEX …` in SQLite,
//! exactly as a single-table covering seek does. graphite's N-table join EQP path
//! hard-coded `USING INDEX` for every index seek, so a covering join seek dropped
//! the `COVERING` label (`SELECT a.x,b.q FROM a JOIN b ON a.x=b.p` with `b(p,q)`
//! indexed → `USING INDEX ib` instead of `USING COVERING INDEX ib`). The check is
//! conservative: a non-covering index still renders the plain `INDEX`, so it never
//! over-claims. Verified byte-for-byte against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn join_covering_index_seek_label_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Each case: an outer table with no usable index (so the driver is
    // unambiguous — the divergence is purely the inner seek's COVERING label).
    let cases = [
        // covering: index holds the selected inner columns
        "CREATE TABLE a(x);CREATE TABLE b(p,q);CREATE INDEX ib ON b(p,q);\
         EXPLAIN QUERY PLAN SELECT a.x,b.q FROM a JOIN b ON a.x=b.p;",
        "CREATE TABLE a(x);CREATE TABLE b(p,q);CREATE INDEX ib ON b(p,q);\
         EXPLAIN QUERY PLAN SELECT * FROM a JOIN b ON a.x=b.p;",
        "CREATE TABLE a(x);CREATE TABLE b(p,q);CREATE INDEX ib ON b(p,q);\
         EXPLAIN QUERY PLAN SELECT b.p FROM a JOIN b ON a.x=b.p;",
        // covering across a LEFT join
        "CREATE TABLE a(x);CREATE TABLE b(p,q);CREATE INDEX ib ON b(p,q);\
         EXPLAIN QUERY PLAN SELECT a.x,b.q FROM a LEFT JOIN b ON a.x=b.p;",
        // covering in a 3-table chain (both inner seeks covering)
        "CREATE TABLE a(x);CREATE TABLE b(p,q);CREATE TABLE c(m,n);\
         CREATE INDEX ib ON b(p,q);CREATE INDEX ic ON c(m,n);\
         EXPLAIN QUERY PLAN SELECT b.q,c.n FROM a JOIN b ON a.x=b.p JOIN c ON a.x=c.m;",
        // NOT covering: a selected inner column is outside the index → plain INDEX
        "CREATE TABLE a(x);CREATE TABLE b(p,q);CREATE INDEX ib ON b(p);\
         EXPLAIN QUERY PLAN SELECT a.x,b.q FROM a JOIN b ON a.x=b.p;",
        "CREATE TABLE a(x);CREATE TABLE b(p,q,r);CREATE INDEX ib ON b(p,q);\
         EXPLAIN QUERY PLAN SELECT b.r FROM a JOIN b ON a.x=b.p;",
        // seek column only → covering (single-column index holding the sole ref)
        "CREATE TABLE a(x);CREATE TABLE b(p,q);CREATE INDEX ib ON b(p);\
         EXPLAIN QUERY PLAN SELECT b.p FROM a JOIN b ON a.x=b.p;",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "mismatch for `{sql}`");
    }
}
