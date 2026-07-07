//! Covering-index preference for a *range-leading* seek (`WHERE a > ?`).
//!
//! Like an equality-prefix seek (see `eqp_seek_index_choice.rs`), SQLite reads a
//! range on an index's leading column straight from a *covering* index when one
//! holds every referenced column — skipping the table b-tree lookup — rather than
//! from a narrower non-covering index. graphite's executor (`try_index_range`) and
//! its `EXPLAIN QUERY PLAN` render (`eqp_access`) share one `choose_range_index`
//! helper, so the plan always matches what runs. Verified byte-for-byte (plan and
//! rows) against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base}{sql}");
    let o = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn plan(bin: &str, base: &str, q: &str) -> String {
    out(bin, base, &format!("EXPLAIN QUERY PLAN {q};"))
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("QUERY PLAN"))
        .map(|l| {
            l.trim_start_matches(|c: char| " |`*+_-".contains(c))
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("#")
}

#[test]
fn range_leading_prefers_covering_index() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // `ia(a)` is narrower; `iab(a,b)` covers a query that also reads `b`. A range on
    // `a` should read from `iab` when `b` is projected, from `ia` (or the covering
    // one) otherwise — matching sqlite either way.
    let cov = "CREATE TABLE t(id INTEGER PRIMARY KEY,a INT,b INT,c TEXT);\
               CREATE INDEX ia ON t(a);CREATE INDEX iab ON t(a,b);\
               INSERT INTO t VALUES(1,1,10,'x'),(2,3,20,'y'),(3,2,30,'z'),(4,5,5,'w');";
    // Same, indexes created in the opposite order (the covering pick must not depend
    // on creation order).
    let cov_rev = "CREATE TABLE t(id INTEGER PRIMARY KEY,a INT,b INT,c TEXT);\
                   CREATE INDEX iab ON t(a,b);CREATE INDEX ia ON t(a);\
                   INSERT INTO t VALUES(1,1,10,'x'),(2,3,20,'y'),(3,2,30,'z');";

    let cases: &[(&str, &str)] = &[
        (cov, "SELECT b FROM t WHERE a>1"),
        (cov, "SELECT a,b FROM t WHERE a>=2"),
        (cov, "SELECT b FROM t WHERE a>1 AND a<5"),
        (cov, "SELECT b FROM t WHERE a<3"),
        (cov_rev, "SELECT b FROM t WHERE a>1"),
        (cov_rev, "SELECT a,b FROM t WHERE a>1"),
    ];
    for (base, q) in cases {
        assert_eq!(plan("sqlite3", base, q), plan(g, base, q), "plan for `{q}`");
        assert_eq!(out("sqlite3", base, q), out(g, base, q), "rows for `{q}`");
    }
}
