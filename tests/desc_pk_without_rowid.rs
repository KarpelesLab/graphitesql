//! A `DESC` column in the PRIMARY KEY of a `WITHOUT ROWID` table orders that
//! table's clustered b-tree descending — byte-for-byte compatible with SQLite.
//! (A `WITHOUT ROWID` table *is* its PK-clustered b-tree, so a declared `DESC`
//! changes both the query order and the on-disk layout.) This exercises the
//! surface differentially against the sqlite3 3.50.4 CLI:
//!
//! * **query order** — `SELECT a`, `SELECT *`, `WHERE a=?`, table-level
//!   `PRIMARY KEY(a DESC)`, column-level `a PRIMARY KEY DESC`, and a mixed
//!   composite `PRIMARY KEY(a, b DESC)` — rows byte-exact vs sqlite;
//! * **ORDER BY** both directions — rows *and* EQP (the clustered walk elides the
//!   sorter for the order it already yields, matching sqlite);
//! * **range on the DESC PK column** — rows byte-exact (the range seek is a
//!   documented deferral for a DESC leading PK, so the rows come from a scan +
//!   WHERE re-filter — we only assert the rows); and
//! * **on-disk byte compatibility** — graphite writes a real FILE containing a
//!   DESC PK (table-level, column-level, AND composite) `WITHOUT ROWID` table,
//!   then the sqlite3 CLI opens THAT file and (a) `PRAGMA integrity_check` is
//!   exactly `ok`, (b) cross-engine reads agree (sqlite reading graphite's file
//!   == graphite reading the same file). This proves the stored clustered b-tree
//!   is sqlite-readable and not corrupted.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run a batch of SQL against a fresh `:memory:` db in the given binary, stdout.
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

/// Table-level `PRIMARY KEY(a DESC)`.
const T_LEVEL: &str = "\
CREATE TABLE t(a,b,PRIMARY KEY(a DESC)) WITHOUT ROWID;\
INSERT INTO t VALUES(1,'x'),(3,'y'),(2,'z'),(4,'w');";

/// Column-level `a PRIMARY KEY DESC`.
const C_LEVEL: &str = "\
CREATE TABLE t(a PRIMARY KEY DESC, b) WITHOUT ROWID;\
INSERT INTO t VALUES(1,'x'),(3,'y'),(2,'z'),(4,'w');";

/// Mixed composite `PRIMARY KEY(a, b DESC)`.
const COMPOSITE: &str = "\
CREATE TABLE t(a,b,x,PRIMARY KEY(a, b DESC)) WITHOUT ROWID;\
INSERT INTO t VALUES(1,5,'p'),(1,2,'q'),(2,9,'r'),(2,3,'s');";

/// Every query returns byte-identical rows from graphite and sqlite.
#[test]
fn desc_pk_without_rowid_query_order() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases: &[(&str, &[&str])] = &[
        (
            T_LEVEL,
            &[
                "SELECT a FROM t;",
                "SELECT * FROM t;",
                "SELECT a,b FROM t WHERE a=2;",
                "SELECT * FROM t WHERE a=4;",
            ],
        ),
        (
            C_LEVEL,
            &[
                "SELECT a FROM t;",
                "SELECT * FROM t;",
                "SELECT b FROM t WHERE a=3;",
            ],
        ),
        (
            COMPOSITE,
            &[
                "SELECT a,b FROM t;",
                "SELECT * FROM t;",
                "SELECT a,b FROM t WHERE a=1;",
                "SELECT x FROM t WHERE a=2 AND b=9;",
            ],
        ),
    ];
    for (base, queries) in cases {
        for q in *queries {
            let s = run_mem("sqlite3", &format!("{base}{q}"));
            let gg = run_mem(g, &format!("{base}{q}"));
            assert_eq!(s, gg, "rows differ for `{q}` on schema `{base}`");
        }
    }
}

/// `ORDER BY` both directions — rows and EQP must match sqlite. The clustered
/// walk already yields `a DESC`, so `ORDER BY a DESC` needs no sorter while
/// `ORDER BY a` keeps a temp b-tree; sqlite plans the same.
#[test]
fn desc_pk_without_rowid_order_by() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // (query, whether the ORDER BY is a single uniform direction relative to the
    // storage walk). When it is, the sorter is fully elided (or fully reversed)
    // and the EQP matches sqlite exactly. A *mixed* case like `ORDER BY a, b` on
    // `(a, b DESC)` needs sqlite's *partial* sorter (`… LAST TERM OF ORDER BY`),
    // which graphite does not model — a pre-existing divergence — so there we
    // only assert the rows.
    let order_cases: &[(&str, &[(&str, bool)])] = &[
        (
            T_LEVEL,
            &[
                ("SELECT a FROM t ORDER BY a DESC", true),
                ("SELECT a FROM t ORDER BY a", true),
            ],
        ),
        (
            COMPOSITE,
            &[
                // Matches the storage walk (a asc, b desc) → no sorter.
                ("SELECT a,b FROM t ORDER BY a, b DESC", true),
                // Full reverse of the storage walk (a desc, b asc) → reverse only.
                ("SELECT a,b FROM t ORDER BY a DESC, b ASC", true),
                // Mixed vs the storage walk → sqlite's partial sorter (not modelled).
                ("SELECT a,b FROM t ORDER BY a, b", false),
                ("SELECT a,b FROM t ORDER BY a DESC, b DESC", false),
            ],
        ),
    ];
    for (base, queries) in order_cases {
        for (q, uniform) in *queries {
            let s = run_mem("sqlite3", &format!("{base}{q};"));
            let gg = run_mem(g, &format!("{base}{q};"));
            assert_eq!(s, gg, "rows differ for `{q}` on `{base}`");
            if *uniform {
                let sp = plan("sqlite3", base, q);
                let gp = plan(g, base, q);
                assert_eq!(sp, gp, "EQP differs for `{q}` on `{base}`");
            }
        }
    }
}

/// A range on the DESC PK column returns byte-exact rows (via scan + re-filter).
#[test]
fn desc_pk_without_rowid_range() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for q in [
        "SELECT a FROM t WHERE a>=2 AND a<4",
        "SELECT a FROM t WHERE a>2",
        "SELECT a FROM t WHERE a<=3 ORDER BY a DESC",
        "SELECT a FROM t WHERE a BETWEEN 2 AND 3",
    ] {
        let s = run_mem("sqlite3", &format!("{T_LEVEL}{q};"));
        let gg = run_mem(g, &format!("{T_LEVEL}{q};"));
        assert_eq!(s, gg, "range rows differ for `{q}`");
    }
}

/// The schema round-trips: `SELECT sql FROM sqlite_master` prints the `DESC` PK
/// direction exactly as sqlite renders it.
#[test]
fn desc_pk_without_rowid_schema_roundtrip() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for base in [T_LEVEL, C_LEVEL, COMPOSITE] {
        let q = "SELECT sql FROM sqlite_master WHERE type='table';";
        let s = run_mem("sqlite3", &format!("{base}{q}"));
        let gg = run_mem(g, &format!("{base}{q}"));
        assert_eq!(s, gg, "schema SQL differs for `{base}`");
    }
}

/// On-disk byte-compat: graphite writes a WITHOUT ROWID DESC-PK table (table-
/// level, column-level, AND composite) to a real FILE; sqlite opens it,
/// `PRAGMA integrity_check` is `ok`, and cross-engine reads agree.
#[test]
fn desc_pk_without_rowid_file_is_sqlite_compatible() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir();
    let path = dir.join(format!("graphite_desc_pk_wr_{}.db", std::process::id()));
    let path_str = path.to_str().unwrap();
    let _ = std::fs::remove_file(&path);

    let build = "\
CREATE TABLE t(a,b,PRIMARY KEY(a DESC)) WITHOUT ROWID;\
INSERT INTO t VALUES(1,'x'),(3,'y'),(2,'z'),(4,'w');\
CREATE TABLE d(a PRIMARY KEY DESC, b) WITHOUT ROWID;\
INSERT INTO d VALUES(10,'j'),(30,'k'),(20,'l');\
CREATE TABLE c(a,b,x,PRIMARY KEY(a, b DESC)) WITHOUT ROWID;\
INSERT INTO c VALUES(1,5,'p'),(1,2,'q'),(2,9,'r'),(2,3,'s');";
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
        "integrity_check on graphite-written DESC-PK WITHOUT ROWID db was not ok:\n{ic}"
    );

    // (b) Cross-engine reads agree (sqlite reading graphite's file == graphite).
    for query in [
        "SELECT a FROM t",
        "SELECT * FROM t",
        "SELECT * FROM t WHERE a=2",
        "SELECT a FROM d",
        "SELECT a,b FROM c",
        "SELECT * FROM c WHERE a=1",
        "SELECT a FROM t ORDER BY a DESC",
        "SELECT a FROM t ORDER BY a",
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
