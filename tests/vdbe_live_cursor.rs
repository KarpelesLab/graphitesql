//! B5b-2 (live inner cursor): a two-table INNER equi-join whose `ON` binds the
//! inner table's INTEGER PRIMARY KEY (`… JOIN t ON o.x = t.<ipk>`) seeks the
//! single matching inner row with a *live* b-tree cursor (`TableCursor::seek`)
//! instead of materializing and scanning the whole inner table. Only the outer
//! table is scanned; each outer row fetches its one inner row by rowid.
//!
//! `query_vdbe` errors on any fallback to the tree-walker, so a passing
//! `query_vdbe` proves the query routed onto this VDBE path. Correctness rides
//! the superset invariant: after the seek the full `ON` is re-evaluated against
//! the assembled row, so every rowid-coercion corner (`= 2.5`, text/blob keys,
//! `NULL`) is filtered exactly as the materialized cross-product would. Each
//! query is checked three ways: it routes on the VDBE, it equals the tree-walker
//! (`set_use_vdbe(false)`), and it equals the sqlite3 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

const SCHEMA: &str = "CREATE TABLE o(x, y TEXT);\
     INSERT INTO o VALUES(1,'a'),(2,'b'),(3,'c'),(NULL,'n'),(2,'b2'),(2.0,'real'),\
       ('2','txt'),('2abc','junk'),(x'32','blob'),(2.5,'half'),(99,'miss'),(-1,'neg'),\
       (2,'two'),(5,'five');\
     CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);\
     INSERT INTO t VALUES(1,'one'),(2,'two'),(5,'five');";

/// Queries that must route onto the live-inner-cursor path (`o JOIN t ON … =
/// t.id`, with the inner table's INTEGER PRIMARY KEY on one side).
const VDBE_QUERIES: &[&str] = &[
    "SELECT o.x, t.name FROM o JOIN t ON o.x = t.id ORDER BY o.y",
    "SELECT o.x, t.name FROM o JOIN t ON t.id = o.x ORDER BY o.y", // ipk on the left
    "SELECT o.y, t.name FROM o JOIN t ON o.x = t.id WHERE t.name <> 'one' ORDER BY o.y",
    "SELECT * FROM o JOIN t ON o.x = t.id ORDER BY o.y",
    "SELECT o.x * 100 + t.id AS s FROM o JOIN t ON o.x = t.id ORDER BY s",
    "SELECT DISTINCT t.name FROM o JOIN t ON o.x = t.id ORDER BY t.name",
    "SELECT o.x, t.name FROM o JOIN t ON o.x = t.id ORDER BY o.y LIMIT 2 OFFSET 1",
    "SELECT count(*), sum(t.id) FROM o JOIN t ON o.x = t.id",
    "SELECT t.name, count(*) FROM o JOIN t ON o.x = t.id GROUP BY t.name ORDER BY t.name",
    "SELECT o.y FROM o JOIN t ON (o.x) = (t.id) ORDER BY o.y", // parenthesized ON
    // LEFT joins (B5b-2b): each outer row keeps exactly one output row, with the
    // inner side null-padded on a NULL key, a seek miss, or a failed `ON`
    // re-check (`= 2.5` seeks rowid 2 but 2.5 = 2 is false → null-padded).
    "SELECT o.x, t.name FROM o LEFT JOIN t ON o.x = t.id ORDER BY o.y",
    "SELECT o.x, t.name FROM o LEFT JOIN t ON t.id = o.x ORDER BY o.y",
    "SELECT o.y, t.id, t.name FROM o LEFT JOIN t ON o.x = t.id ORDER BY o.y",
    "SELECT o.y FROM o LEFT JOIN t ON o.x = t.id WHERE t.name IS NULL ORDER BY o.y",
    "SELECT count(*), count(t.id) FROM o LEFT JOIN t ON o.x = t.id",
    "SELECT o.x, t.name FROM o LEFT JOIN t ON o.x = t.id ORDER BY o.y LIMIT 3 OFFSET 2",
    // Compound `ON` (B5b-2c): the ipk equality is one `AND` conjunct — it drives
    // the rowid seek, the whole `ON` is re-checked so the extra conjunct just
    // filters the seeked row. The ipk-eq may be either conjunct.
    "SELECT o.x, t.name FROM o JOIN t ON o.x = t.id AND t.name = o.y ORDER BY o.y",
    "SELECT o.x, t.name FROM o JOIN t ON t.name = o.y AND o.x = t.id ORDER BY o.y",
    "SELECT o.x, t.name FROM o JOIN t ON o.x = t.id AND t.name <> 'one' ORDER BY o.y",
    "SELECT o.x, t.name FROM o LEFT JOIN t ON o.x = t.id AND t.name = o.y ORDER BY o.y",
    "SELECT count(*) FROM o JOIN t ON o.x = t.id AND o.y = t.name",
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
fn live_inner_cursor_routes_and_matches_tree_walker() {
    let c = setup();
    for q in VDBE_QUERIES {
        // Routes onto the VDBE (errors on any fallback).
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
fn live_inner_cursor_matches_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    // Compare the graphite CLI binary (VDBE enabled by default, so the seek path
    // runs) against sqlite3 end-to-end — float/blob rendering included, so no
    // hand-rolled value formatting can mask a divergence.
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in VDBE_QUERIES {
        assert_eq!(
            cli_rows("sqlite3", q),
            cli_rows(g, q),
            "sqlite3 vs graphite `{q}`"
        );
    }
}
