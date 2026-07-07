//! A `DESC` column in a `UNIQUE(col …)` constraint, or in a rowid table's
//! `PRIMARY KEY(col …)` (which builds an auto `UNIQUE` index because the PK is
//! not the integer rowid alias), orders that constraint's automatic
//! `sqlite_autoindex_*` b-tree descending — byte-for-byte compatible with SQLite.
//! Before this fix the auto-index was always stored/seeked ascending, so both the
//! query order AND the on-disk bytes disagreed with sqlite. This exercises the
//! surface differentially against the sqlite3 3.50.4 CLI:
//!
//! * **query order** — equality, range, and covering shapes over `UNIQUE(a DESC)`,
//!   composite `UNIQUE(a, b DESC)`, rowid-table `PRIMARY KEY(a DESC)` and
//!   `PRIMARY KEY(a, b DESC)` — rows byte-exact vs sqlite;
//! * **ORDER BY** both directions where the auto-index can serve it — rows *and*
//!   EQP (the covering-index walk elides the sorter for the order it already
//!   yields, matching sqlite);
//! * **schema round-trip** — `SELECT sql FROM sqlite_master` prints `UNIQUE(a DESC)`
//!   exactly as sqlite renders it; and
//! * **on-disk byte compatibility** — graphite writes a real FILE containing a
//!   `UNIQUE(a DESC)` table AND a rowid-table `PRIMARY KEY(a DESC)` table, then
//!   the sqlite3 CLI opens THAT file and (a) `PRAGMA integrity_check` is exactly
//!   `ok`, (b) cross-engine reads agree (sqlite reading graphite's file ==
//!   graphite reading the same file). This proves the stored auto-index b-tree is
//!   sqlite-readable and not corrupted.

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

/// `UNIQUE(a DESC)` on a rowid table (integer PK is the rowid; `a` gets an auto
/// UNIQUE index ordered descending).
const U_DESC: &str = "\
CREATE TABLE t(id INTEGER PRIMARY KEY, a, UNIQUE(a DESC));\
INSERT INTO t VALUES(1,30),(2,10),(3,20),(4,40);";

/// Composite `UNIQUE(a, b DESC)` — trailing DESC column.
const U_COMPOSITE: &str = "\
CREATE TABLE t(id INTEGER PRIMARY KEY, a, b, UNIQUE(a, b DESC));\
INSERT INTO t VALUES(1,1,30),(2,1,10),(3,1,20),(4,2,5),(5,2,9);";

/// Rowid-table `PRIMARY KEY(a DESC)` — non-integer PK ⇒ auto UNIQUE index on `a`
/// (descending); the table itself is still a rowid table.
const P_DESC: &str = "\
CREATE TABLE t(a, b, PRIMARY KEY(a DESC));\
INSERT INTO t VALUES(1,'x'),(3,'y'),(2,'z'),(4,'w');";

/// Rowid-table composite `PRIMARY KEY(a, b DESC)`.
const P_COMPOSITE: &str = "\
CREATE TABLE t(a, b, c, PRIMARY KEY(a, b DESC));\
INSERT INTO t VALUES(1,30,'p'),(1,10,'q'),(1,20,'r'),(2,5,'s');";

/// Every query returns byte-identical rows from graphite and sqlite — equality,
/// range, and covering shapes.
#[test]
fn desc_auto_index_query_order() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases: &[(&str, &[&str])] = &[
        (
            U_DESC,
            &[
                // Covering scan over the auto-index → a DESC.
                "SELECT a FROM t WHERE a>5;",
                "SELECT a FROM t WHERE a>=20;",
                "SELECT a FROM t WHERE a<30;",
                "SELECT a FROM t WHERE a=20;",
                "SELECT a FROM t WHERE a BETWEEN 15 AND 35;",
                "SELECT a FROM t;",
            ],
        ),
        (
            U_COMPOSITE,
            &[
                "SELECT a,b FROM t WHERE a=1;",
                "SELECT b FROM t WHERE a=2;",
                "SELECT a,b FROM t WHERE a=1 AND b>15;",
                "SELECT a,b FROM t;",
            ],
        ),
        (
            P_DESC,
            &[
                "SELECT a FROM t WHERE a>0;",
                "SELECT a FROM t WHERE a>=2;",
                "SELECT a FROM t WHERE a=3;",
                "SELECT a,b FROM t WHERE a<3;",
                "SELECT a FROM t;",
            ],
        ),
        (
            P_COMPOSITE,
            &[
                "SELECT a,b FROM t WHERE a=1;",
                "SELECT b FROM t WHERE a=1 AND b>15;",
                "SELECT a,b FROM t;",
                "SELECT c FROM t WHERE a=1 AND b=20;",
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

/// `ORDER BY` both directions where the auto-index can serve it — rows *and* EQP
/// must match sqlite. The covering-index walk already yields `a DESC`, so
/// `ORDER BY a DESC` needs no sorter while `ORDER BY a` reverses the walk;
/// sqlite plans the same.
#[test]
fn desc_auto_index_order_by() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // (query, whether the ORDER BY is a single uniform direction relative to the
    // index walk). When it is, the sorter is elided (or fully reversed) and the
    // EQP matches sqlite exactly. A *mixed* case vs the (a asc, b desc) walk needs
    // sqlite's partial sorter, which graphite does not model, so there we only
    // assert the rows.
    let order_cases: &[(&str, &[(&str, bool)])] = &[
        (
            U_DESC,
            &[
                ("SELECT a FROM t ORDER BY a DESC", true),
                ("SELECT a FROM t ORDER BY a", true),
            ],
        ),
        (
            U_COMPOSITE,
            &[
                // Matches the index walk (a asc, b desc) → no sorter.
                ("SELECT a,b FROM t ORDER BY a, b DESC", true),
                // Full reverse of the index walk (a desc, b asc) → reverse only.
                ("SELECT a,b FROM t ORDER BY a DESC, b ASC", true),
                // Mixed vs the index walk → sqlite's partial sorter (not modelled).
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

/// A `UNIQUE(b DESC)` auto-index on a *`WITHOUT ROWID`* table (whose auto-index
/// is the second `sqlite_autoindex_*`, the clustered PK being the first). The
/// leading-column range seek honours the DESC direction (swapping the value-space
/// bounds), so the covering-index rows arrive in `b DESC` order — byte-exact vs
/// sqlite. Rows only: the ORDER-BY-elision EQP for a WITHOUT ROWID DESC secondary
/// index is a documented pre-existing deferral (a temp b-tree still sorts), which
/// is out of scope here — the rows are correct regardless.
#[test]
fn desc_auto_index_without_rowid_secondary() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let base = "\
CREATE TABLE t(a PRIMARY KEY, b, UNIQUE(b DESC)) WITHOUT ROWID;\
INSERT INTO t VALUES(1,30),(2,10),(3,20),(4,40);";
    for q in [
        "SELECT b FROM t WHERE b>5",
        "SELECT b FROM t WHERE b>=20",
        "SELECT b FROM t WHERE b<30",
        "SELECT b FROM t WHERE b BETWEEN 15 AND 35",
        "SELECT a,b FROM t WHERE b>15",
    ] {
        let s = run_mem("sqlite3", &format!("{base}{q};"));
        let gg = run_mem(g, &format!("{base}{q};"));
        assert_eq!(s, gg, "rows differ for `{q}` on `{base}`");
    }
}

/// The schema round-trips: `SELECT sql FROM sqlite_master` prints the `DESC`
/// direction of a `UNIQUE(a DESC)` constraint exactly as sqlite renders it.
#[test]
fn desc_auto_index_schema_roundtrip() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for base in [U_DESC, U_COMPOSITE, P_DESC, P_COMPOSITE] {
        let q = "SELECT sql FROM sqlite_master WHERE type='table';";
        let s = run_mem("sqlite3", &format!("{base}{q}"));
        let gg = run_mem(g, &format!("{base}{q}"));
        assert_eq!(s, gg, "schema SQL differs for `{base}`");
    }
}

/// On-disk byte-compat: graphite writes a `UNIQUE(a DESC)` table AND a rowid-table
/// `PRIMARY KEY(a DESC)` table (plus composite forms) to a real FILE; sqlite opens
/// it, `PRAGMA integrity_check` is `ok`, and cross-engine reads agree.
#[test]
fn desc_auto_index_file_is_sqlite_compatible() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir();
    let path = dir.join(format!("graphite_desc_auto_idx_{}.db", std::process::id()));
    let path_str = path.to_str().unwrap();
    let _ = std::fs::remove_file(&path);

    let build = "\
CREATE TABLE u(id INTEGER PRIMARY KEY, a, UNIQUE(a DESC));\
INSERT INTO u VALUES(1,30),(2,10),(3,20),(4,40);\
CREATE TABLE uc(id INTEGER PRIMARY KEY, a, b, UNIQUE(a, b DESC));\
INSERT INTO uc VALUES(1,1,30),(2,1,10),(3,1,20),(4,2,5);\
CREATE TABLE p(a, b, PRIMARY KEY(a DESC));\
INSERT INTO p VALUES(1,'x'),(3,'y'),(2,'z'),(4,'w');\
CREATE TABLE pc(a, b, c, PRIMARY KEY(a, b DESC));\
INSERT INTO pc VALUES(1,30,'p'),(1,10,'q'),(1,20,'r'),(2,5,'s');";
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
        "integrity_check on graphite-written DESC auto-index db was not ok:\n{ic}"
    );

    // (b) Cross-engine reads agree (sqlite reading graphite's file == graphite).
    for query in [
        "SELECT a FROM u WHERE a>5",
        "SELECT a FROM u ORDER BY a DESC",
        "SELECT a FROM u ORDER BY a",
        "SELECT a,b FROM uc WHERE a=1",
        "SELECT a FROM p WHERE a>0",
        "SELECT a FROM p ORDER BY a DESC",
        "SELECT * FROM p WHERE a=3",
        "SELECT a,b FROM pc WHERE a=1",
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
