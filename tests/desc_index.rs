//! `DESC` (descending) index columns are stored in the b-tree in REVERSED value
//! order — byte-for-byte compatible with SQLite. This exercises the whole DESC
//! index surface differentially against the sqlite3 3.50.4 CLI:
//!
//! * **query order** — equality/covering seeks, non-covering seeks, single vs
//!   composite indexes, a 3-column index, a NULL-keyed prefix, a DESC *leading*
//!   column, and a mixed `(a DESC, b)` index — rows byte-exact vs sqlite;
//! * **ORDER BY** — `ORDER BY b DESC` / `ORDER BY b ASC` on a `(a, b DESC)` index
//!   (rows and EQP);
//! * **range on a DESC column** — `b>10`, `b<25`, `b BETWEEN …` (rows byte-exact;
//!   the range-seek EXPLAIN may fall back to the equality-prefix form, a
//!   documented deferral — we only assert the rows here); and
//! * **on-disk byte compatibility** — graphite writes a real FILE containing DESC
//!   indexes, then the sqlite3 CLI opens THAT file and (a) `PRAGMA
//!   integrity_check` is exactly `ok`, (b) a query over the DESC index returns the
//!   same rows graphite does. This proves the stored index is sqlite-readable.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run a batch of SQL against a fresh `:memory:` db in the given binary and return
/// stdout verbatim.
fn run_mem(bin: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run a query against an existing FILE and return stdout verbatim.
fn run_file(bin: &str, path: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(path).arg(sql).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Normalize an EQP dump (drop the header and tree glyphs) for comparison.
fn plan(bin: &str, base: &str, sql: &str) -> String {
    let full = format!("{base} EXPLAIN QUERY PLAN {sql}");
    run_mem(bin, &full)
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|c: char| " |`*+_-".contains(c)))
        .collect::<Vec<_>>()
        .join("#")
}

const SCHEMA: &str = "\
CREATE TABLE t(id INTEGER PRIMARY KEY, a, b, w);\
CREATE INDEX iab ON t(a, b DESC);\
INSERT INTO t VALUES\
 (1,5,30,'x'),(2,5,10,'y'),(3,5,20,'z'),\
 (4,7,1,'p'),(5,7,9,'q'),\
 (6,5,NULL,'n1'),(7,5,NULL,'n2'),\
 (8,NULL,3,'m');";

/// Every query below must return byte-identical rows from graphite and sqlite.
#[test]
fn desc_index_query_rows_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");

    // (schema, query). Each schema is self-contained so cases don't interfere.
    let cases: &[(&str, &str)] = &[
        // Composite (a, b DESC): equality/covering seek — b comes out DESC.
        (SCHEMA, "SELECT b FROM t WHERE a=5"),
        // Non-covering (w is not in the index → fetch by rowid).
        (SCHEMA, "SELECT b, w FROM t WHERE a=5"),
        // NULL-keyed prefix (a=5 rows include NULL b).
        (SCHEMA, "SELECT b FROM t WHERE a=5 ORDER BY 1"),
        // A different equality key.
        (SCHEMA, "SELECT b FROM t WHERE a=7"),
        // Single-column DESC index, equality seek (direction-agnostic).
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY,a);CREATE INDEX ia ON t(a DESC);\
             INSERT INTO t VALUES(1,10),(2,30),(3,20),(4,NULL),(5,25),(6,30);",
            "SELECT id FROM t WHERE a=30 ORDER BY id",
        ),
        // Single-column DESC index, range on the DESC leading column: the seek
        // walks the index (value-space bounds swapped) and yields rows DESC, exactly
        // like sqlite — no ORDER BY needed to be deterministic.
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY,a);CREATE INDEX ia ON t(a DESC);\
             INSERT INTO t VALUES(1,10),(2,30),(3,20),(4,NULL),(5,25);",
            "SELECT a FROM t WHERE a>15",
        ),
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY,a);CREATE INDEX ia ON t(a DESC);\
             INSERT INTO t VALUES(1,10),(2,30),(3,20),(4,NULL),(5,25);",
            "SELECT a FROM t WHERE a BETWEEN 10 AND 25",
        ),
        // DESC *leading* column of a composite index: equality on the 2nd column
        // is not a prefix seek, but a range/scan still returns correct rows.
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);CREATE INDEX iab ON t(a DESC,b);\
             INSERT INTO t VALUES(1,5,1),(2,3,2),(3,9,3),(4,5,4),(5,7,5);",
            "SELECT a,b FROM t WHERE a=5",
        ),
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);CREATE INDEX iab ON t(a DESC,b);\
             INSERT INTO t VALUES(1,5,1),(2,3,2),(3,9,3),(4,5,4),(5,7,5);",
            "SELECT a,b FROM t ORDER BY a DESC, b",
        ),
        // Mixed (a DESC, b): full walk in each ORDER BY direction combination.
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b);CREATE INDEX iab ON t(a DESC,b);\
             INSERT INTO t VALUES(1,5,1),(2,3,2),(3,9,3),(4,5,4),(5,7,5);",
            "SELECT a,b FROM t ORDER BY a, b DESC",
        ),
        // 3-column index with a DESC in the middle.
        (
            "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b,c);CREATE INDEX iabc ON t(a,b DESC,c);\
             INSERT INTO t VALUES\
              (1,1,5,100),(2,1,5,50),(3,1,9,10),(4,1,9,20),(5,2,1,1);",
            "SELECT b,c FROM t WHERE a=1",
        ),
        // ORDER BY on the DESC column, both directions.
        (SCHEMA, "SELECT b FROM t WHERE a=5 ORDER BY b DESC"),
        (SCHEMA, "SELECT b FROM t WHERE a=5 ORDER BY b ASC"),
        // Range on a DESC column.
        (SCHEMA, "SELECT b FROM t WHERE a=5 AND b>10"),
        (SCHEMA, "SELECT b FROM t WHERE a=5 AND b<25"),
        (SCHEMA, "SELECT b FROM t WHERE a=5 AND b BETWEEN 10 AND 25"),
        (
            SCHEMA,
            "SELECT b FROM t WHERE a=5 AND b>=10 ORDER BY b DESC",
        ),
    ];

    for (schema, query) in cases {
        let sql = format!("{schema}\n{query};");
        let g_rows = run_mem(g, &sql);
        let s_rows = run_mem("sqlite3", &sql);
        assert_eq!(
            g_rows, s_rows,
            "rows differ for `{query}`\n  graphite: {g_rows:?}\n  sqlite:   {s_rows:?}"
        );
    }
}

/// ORDER BY over a DESC index column: rows AND the EXPLAIN QUERY PLAN must match
/// (both directions are served by the index — no temp b-tree).
#[test]
fn desc_index_order_by_plan_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases: &[&str] = &[
        "SELECT b FROM t WHERE a=5 ORDER BY b DESC",
        "SELECT b FROM t WHERE a=5 ORDER BY b ASC",
    ];
    for query in cases {
        let g_plan = plan(g, SCHEMA, query);
        let s_plan = plan("sqlite3", SCHEMA, query);
        assert_eq!(g_plan, s_plan, "EQP differs for `{query}`");
        let sql = format!("{SCHEMA}\n{query};");
        assert_eq!(run_mem(g, &sql), run_mem("sqlite3", &sql));
    }

    // No-WHERE covering scans over a mixed `(a, b DESC)` index: `ORDER BY a, b
    // DESC` is fully served (no temp b-tree); `ORDER BY a, b` needs one.
    for query in [
        "SELECT a,b FROM t ORDER BY a, b DESC",
        "SELECT a,b FROM t ORDER BY a, b",
        "SELECT a,b FROM t ORDER BY a DESC, b",
        "SELECT a,b FROM t ORDER BY a DESC, b DESC",
    ] {
        assert_eq!(
            plan(g, SCHEMA, query),
            plan("sqlite3", SCHEMA, query),
            "EQP differs for `{query}`"
        );
        let sql = format!("{SCHEMA}\n{query};");
        assert_eq!(
            run_mem(g, &sql),
            run_mem("sqlite3", &sql),
            "rows differ for `{query}`"
        );
    }
}

/// An equality prefix followed by a range on a DESC column (`x=? AND y>?` over an
/// index on `(x, y DESC)`) is seeked as one bounded index range — the value-space
/// bounds are swapped for the DESC column — so BOTH the rows and the EXPLAIN QUERY
/// PLAN match sqlite (the range condition is rendered, not deferred).
#[test]
fn desc_second_column_range_seek_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let schema = "CREATE TABLE d(x,y,z);CREATE INDEX id ON d(x ASC, y DESC);\
         INSERT INTO d VALUES (1,10,'a'),(1,20,'b'),(1,30,'e'),(1,5,'f'),(2,5,'c');";
    for query in [
        "SELECT z,y FROM d WHERE x=1 AND y>5",
        "SELECT z,y FROM d WHERE x=1 AND y>=10",
        "SELECT z,y FROM d WHERE x=1 AND y<25",
        "SELECT z,y FROM d WHERE x=1 AND y>5 AND y<25",
        "SELECT z,y FROM d WHERE x=1 AND y BETWEEN 10 AND 25",
        "SELECT y FROM d WHERE x=1 AND y>5 ORDER BY y ASC",
    ] {
        assert_eq!(
            plan(g, schema, query),
            plan("sqlite3", schema, query),
            "EQP differs for `{query}`"
        );
        let sql = format!("{schema}\n{query};");
        assert_eq!(
            run_mem(g, &sql),
            run_mem("sqlite3", &sql),
            "rows differ for `{query}`"
        );
    }
}

/// A range on a DESC *leading* index column also seeks (bounds swapped into
/// key-sort order), so BOTH rows and EQP match sqlite — the walk yields rows in
/// value-descending order without an ORDER BY.
#[test]
fn desc_leading_column_range_seek_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let schema = "CREATE TABLE t(id INTEGER PRIMARY KEY,a);CREATE INDEX ia ON t(a DESC);\
         INSERT INTO t VALUES(1,10),(2,30),(3,20),(4,NULL),(5,25),(6,5);";
    for query in [
        "SELECT a FROM t WHERE a>15",
        "SELECT a FROM t WHERE a<25",
        "SELECT a FROM t WHERE a>=10 AND a<=25",
        "SELECT a FROM t WHERE a BETWEEN 10 AND 25",
        // Trailing rowid stays ASC under the DESC walk, so `ORDER BY a, id` needs a
        // temp b-tree for `id` — matching sqlite's plan.
        "SELECT a,id FROM t WHERE a>2 ORDER BY a, id",
    ] {
        assert_eq!(
            plan(g, schema, query),
            plan("sqlite3", schema, query),
            "EQP differs for `{query}`"
        );
        let sql = format!("{schema}\n{query};");
        assert_eq!(
            run_mem(g, &sql),
            run_mem("sqlite3", &sql),
            "rows differ for `{query}`"
        );
    }
}

/// The KEY guard: graphite writes DESC indexes to a real FILE; the sqlite3 CLI
/// then opens that file, passes `PRAGMA integrity_check`, and reads back the same
/// rows. Proves the on-disk DESC index is well-formed and sqlite-readable.
#[test]
fn desc_index_file_is_sqlite_compatible() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");

    let dir = std::env::temp_dir();
    let path = dir.join(format!("graphite_desc_idx_{}.db", std::process::id()));
    let path_str = path.to_str().unwrap();
    // Clean any stale file from a previous crashed run.
    let _ = std::fs::remove_file(&path);

    // Build the database in graphite, writing to the file. Two indexes: a trailing
    // DESC column and a leading DESC column (mixed directions).
    let build = "\
CREATE TABLE t(id INTEGER PRIMARY KEY, a, b);\
CREATE INDEX iab ON t(a, b DESC);\
CREATE INDEX iba ON t(b DESC, a);\
INSERT INTO t VALUES\
 (1,5,30),(2,5,10),(3,5,20),(4,7,1),(5,7,9),(6,5,NULL),(7,NULL,4);";
    let out = Command::new(g).arg(path_str).arg(build).output().unwrap();
    assert!(
        out.status.success(),
        "graphite build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // (a) sqlite integrity_check on the graphite-written file must be exactly "ok".
    let ic = run_file("sqlite3", path_str, "PRAGMA integrity_check;");
    assert_eq!(
        ic.trim(),
        "ok",
        "integrity_check on graphite-written DESC index db was not ok:\n{ic}"
    );

    // (b) A query served by the DESC index returns identical rows from sqlite
    //     (reading graphite's file) and graphite (reading the same file).
    for query in [
        "SELECT b FROM t WHERE a=5",
        "SELECT a FROM t WHERE b=10",
        "SELECT b FROM t WHERE a=5 ORDER BY b DESC",
        "SELECT id,a,b FROM t ORDER BY b DESC, a",
    ] {
        let s_rows = run_file("sqlite3", path_str, &format!("{query};"));
        let g_rows = run_file(g, path_str, &format!("{query};"));
        assert_eq!(
            s_rows, g_rows,
            "sqlite (reading graphite's file) and graphite disagree for `{query}`"
        );
    }

    let _ = std::fs::remove_file(&path);
}
