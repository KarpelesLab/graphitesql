//! B5b-2 (live inner cursor, secondary index): an INNER/LEFT equi-join whose
//! `ON` binds a joined table's single-column **UNIQUE** index (`… JOIN t ON o.x =
//! t.c`, `c` UNIQUE) seeks the single matching inner row with a *live* index
//! cursor (`index_seek_fetch` → `TableCursor::seek`) instead of materializing and
//! scanning the whole inner table — the same live-cursor path the INTEGER PRIMARY
//! KEY seek uses, generalized to any single-column unique BINARY index (an inline
//! `UNIQUE`/`PRIMARY KEY` autoindex or a `CREATE UNIQUE INDEX`).
//!
//! Uniqueness keeps the "≤ 1 inner row per outer row" invariant the null-pad /
//! `ON`-re-check logic relies on. The path is guarded to equal affinity on the
//! two sides and BINARY collation on both the column and the index, so the index
//! seek and the re-checked `ON` `=` agree exactly (no dropped matches). It
//! generalizes to a bounded left-deep chain, mixing rowid and unique-index seeks.
//!
//! `query_vdbe` errors on any fallback to the tree-walker, so a passing
//! `query_vdbe` proves the query routed onto this VDBE path. Each query is checked
//! three ways: it routes on the VDBE, it equals the tree-walker
//! (`set_use_vdbe(false)`), and it equals the sqlite3 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

// `t.c` is an inline UNIQUE column (INTEGER affinity, BINARY); `t.d` is covered
// by a standalone `CREATE UNIQUE INDEX` (TEXT affinity). `o.x`/`o.w` match those
// affinities so the equal-affinity guard admits the index seek.
const SCHEMA: &str = "CREATE TABLE o(x INTEGER, w TEXT, y TEXT);\
     INSERT INTO o VALUES(10,'p','a'),(20,'q','b'),(30,'z','c'),(NULL,'q','n'),\
       (20,'p','b2'),(99,'q','miss'),(-5,'z','neg'),(20,'r','three');\
     CREATE TABLE t(k INTEGER PRIMARY KEY, c INTEGER UNIQUE, d TEXT, nm TEXT);\
     INSERT INTO t VALUES(1,10,'p','ten'),(2,20,'q','twenty'),(3,50,'z','fifty');\
     CREATE UNIQUE INDEX ud ON t(d);";

/// Queries that must route onto the live unique-index-seek path.
const VDBE_QUERIES: &[&str] = &[
    // Inline UNIQUE column `c` (INTEGER), either operand order.
    "SELECT o.y, t.nm FROM o JOIN t ON o.x = t.c ORDER BY o.y",
    "SELECT o.y, t.nm FROM o JOIN t ON t.c = o.x ORDER BY o.y",
    "SELECT * FROM o JOIN t ON o.x = t.c ORDER BY o.y",
    "SELECT o.y, t.c, t.nm FROM o LEFT JOIN t ON o.x = t.c ORDER BY o.y",
    "SELECT o.y FROM o LEFT JOIN t ON o.x = t.c WHERE t.nm IS NULL ORDER BY o.y",
    "SELECT count(*), count(t.c) FROM o LEFT JOIN t ON o.x = t.c",
    "SELECT o.y, t.nm FROM o JOIN t ON o.x = t.c ORDER BY o.y LIMIT 2 OFFSET 1",
    "SELECT t.nm, count(*) FROM o JOIN t ON o.x = t.c GROUP BY t.nm ORDER BY t.nm",
    // Compound `ON`: the unique-column equality drives the seek, the extra
    // conjunct just filters the seeked row (either conjunct order).
    "SELECT o.y, t.nm FROM o JOIN t ON o.x = t.c AND t.nm <> 'ten' ORDER BY o.y",
    "SELECT o.y, t.nm FROM o JOIN t ON t.nm <> 'ten' AND o.x = t.c ORDER BY o.y",
    // Standalone `CREATE UNIQUE INDEX` over a TEXT column, matched to `o.w`.
    "SELECT o.y, t.nm FROM o JOIN t ON o.w = t.d ORDER BY o.y, t.nm",
    "SELECT o.y, t.nm FROM o LEFT JOIN t ON o.w = t.d ORDER BY o.y, t.nm",
];

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SCHEMA.split(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

#[test]
fn unique_index_seek_routes_and_matches_tree_walker() {
    let c = setup();
    for q in VDBE_QUERIES {
        // Routes onto the VDBE (errors on any fallback to the tree-walker).
        let vdbe = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE routing for `{q}`: {e:?}"));
        // Tree-walker (source of truth).
        c.set_use_vdbe(false);
        let tw = c.query(q).unwrap();
        c.set_use_vdbe(true);
        assert_eq!(vdbe.rows, tw.rows, "VDBE vs tree-walker rows for `{q}`");
        assert_eq!(vdbe.columns, tw.columns, "columns for `{q}`");
    }
}

fn cli_rows(bin: &str, sql: &str) -> String {
    let full = format!("{SCHEMA} {sql}");
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn unique_index_seek_matches_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in VDBE_QUERIES {
        assert_eq!(
            cli_rows("sqlite3", q),
            cli_rows(g, q),
            "sqlite3 vs graphite `{q}`"
        );
    }
}

// A three-table chain mixing a unique-index seek (`o.x = t.c`) with a rowid seek
// (`t.k = u.uid`): the first inner is fetched by its UNIQUE index, the second by
// its INTEGER PRIMARY KEY off the first seeked inner.
const CHAIN: &str = "CREATE TABLE o(x INTEGER, y TEXT);\
     INSERT INTO o VALUES(10,'a'),(20,'b'),(30,'c'),(NULL,'n'),(20,'b2'),(99,'m');\
     CREATE TABLE t(k INTEGER PRIMARY KEY, c INTEGER UNIQUE, tn TEXT);\
     INSERT INTO t VALUES(1,10,'t1'),(2,20,'t2'),(5,50,'t5');\
     CREATE TABLE u(uid INTEGER PRIMARY KEY, un TEXT);\
     INSERT INTO u VALUES(1,'u1'),(2,'u2'),(5,'u5');";

const CHAIN_QUERIES: &[&str] = &[
    "SELECT o.y, t.tn, u.un FROM o JOIN t ON o.x = t.c JOIN u ON t.k = u.uid ORDER BY o.y",
    "SELECT o.y, u.un FROM o JOIN t ON o.x = t.c JOIN u ON t.k = u.uid WHERE u.un <> 'u5' ORDER BY o.y",
    "SELECT count(*) FROM o JOIN t ON o.x = t.c JOIN u ON t.k = u.uid",
];

fn setup_chain() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in CHAIN.split(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

#[test]
fn mixed_chain_routes_and_matches_tree_walker() {
    let c = setup_chain();
    for q in CHAIN_QUERIES {
        let vdbe = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE routing for `{q}`: {e:?}"));
        c.set_use_vdbe(false);
        let tw = c.query(q).unwrap();
        c.set_use_vdbe(true);
        assert_eq!(vdbe.rows, tw.rows, "VDBE vs tree-walker rows for `{q}`");
    }
}

#[test]
fn mixed_chain_matches_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in CHAIN_QUERIES {
        let full = format!("{CHAIN} {q}");
        let sq = Command::new("sqlite3")
            .arg(":memory:")
            .arg(&full)
            .output()
            .unwrap();
        let gr = Command::new(g).arg(":memory:").arg(&full).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&sq.stdout),
            String::from_utf8_lossy(&gr.stdout),
            "sqlite3 vs graphite `{q}`"
        );
    }
}
