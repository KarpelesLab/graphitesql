//! An explicit `COLLATE` on a comparison operand must change only the collating
//! sequence, never the operand's type affinity. graphite's `expr_affinity` was
//! missing the `COLLATE` case, so a `COLLATE`-wrapped *typeless* column lost its
//! (BLOB/none) affinity and was treated like an affinity-less literal. That
//! wrongly enabled the "TEXT vs no-affinity literal" coercion rule, so e.g.
//! `text_col = none_col COLLATE NOCASE` spuriously coerced `1` to `'1'` and
//! matched — while `text_col = none_col` (no COLLATE) correctly did not. The bug
//! surfaced most visibly on LEFT/RIGHT joins (a spurious match instead of a
//! null-padded row). Verified byte-for-byte against the sqlite3 3.50.4 CLI
//! (found by a unique-index-join fuzzer, minimized here).

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
fn collate_does_not_change_comparison_affinity() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");

    // `o.x` is a TEXT-affinity column holding text '1'; `t.c` is a typeless
    // (no-affinity) column holding integer 1. `'1' = 1` is false across these
    // columns whether or not a COLLATE is present — the COLLATE must not enable
    // the literal-only TEXT coercion.
    let base = "CREATE TABLE t(c, nm TEXT);INSERT INTO t VALUES(1,'n');\
        CREATE TABLE o(x TEXT);INSERT INTO o VALUES('1');";
    let queries = [
        // joins — the null-pad path made the spurious match most visible
        "SELECT o.x,t.c,t.nm FROM o LEFT JOIN t ON o.x = t.c",
        "SELECT o.x,t.c,t.nm FROM o LEFT JOIN t ON o.x = t.c COLLATE NOCASE",
        "SELECT o.x,t.c,t.nm FROM o LEFT JOIN t ON o.x = t.c COLLATE BINARY",
        "SELECT o.x,t.c,t.nm FROM o LEFT JOIN t ON o.x COLLATE NOCASE = t.c",
        "SELECT o.x,t.c,t.nm FROM o JOIN t ON o.x = t.c COLLATE NOCASE",
        "SELECT o.x,t.c,t.nm FROM o RIGHT JOIN t ON o.x = t.c COLLATE NOCASE",
        // single-table comparison contexts
        "SELECT o.x FROM o, t WHERE o.x = t.c COLLATE NOCASE",
        "SELECT (o.x = t.c COLLATE NOCASE) FROM o, t",
        "SELECT o.x FROM o, t WHERE o.x COLLATE RTRIM = t.c",
    ];
    let mut sql = String::new();
    for q in queries {
        sql.push_str(base);
        sql.push_str(q);
        sql.push_str(";DROP TABLE o;DROP TABLE t;");
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));

    // The mirror case: a NUMERIC-affinity column vs a typeless column with
    // COLLATE still applies numeric coercion (rule 1 is unaffected).
    let num = "CREATE TABLE t(a INTEGER, c, y TEXT);\
        INSERT INTO t VALUES(1,'1','r1'),(2,2,'r2'),(3,'x','r3');\
        SELECT y FROM t WHERE a = c COLLATE NOCASE ORDER BY y;";
    assert_eq!(out("sqlite3", num), out(g, num));
}
