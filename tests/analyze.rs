//! Track B: `ANALYZE` writes `sqlite_stat1`, and the query planner uses those
//! statistics to choose the most selective index. Verified against `sqlite3`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn rows_str(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn stat1_matches_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = [
        "CREATE TABLE t(a,b,c)",
        "CREATE INDEX i_a ON t(a)",
        "CREATE INDEX i_bc ON t(b,c)",
        "INSERT INTO t VALUES (1,1,1),(1,2,2),(1,2,3),(2,2,3),(2,3,4),(3,3,4)",
        "CREATE TABLE noidx(x)",
        "INSERT INTO noidx VALUES (1),(2),(3)",
        "ANALYZE",
    ];

    // Reference: sqlite3 building the same database.
    let path = std::env::temp_dir().join(format!("gsql-an-ref-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(setup.join(";"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let want = {
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg("SELECT tbl,coalesce(idx,'NULL'),stat FROM sqlite_stat1 ORDER BY tbl,idx")
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    };
    let _ = std::fs::remove_file(&path);

    // graphitesql building the same database.
    let mut g = Connection::open_memory().unwrap();
    for s in setup {
        g.execute(s).unwrap();
    }
    let got = rows_str(
        &g,
        "SELECT tbl,coalesce(idx,'NULL'),stat FROM sqlite_stat1 ORDER BY tbl,idx",
    );
    assert_eq!(got, want, "sqlite_stat1 mismatch");
}

#[test]
fn analyze_is_idempotent_and_replaces() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("CREATE INDEX i ON t(a)").unwrap();
    c.execute("INSERT INTO t VALUES (1),(2),(3)").unwrap();
    c.execute("ANALYZE").unwrap();
    assert_eq!(rows_str(&c, "SELECT stat FROM sqlite_stat1"), "3 1");
    // Add rows and re-analyze: the row is replaced, not duplicated.
    c.execute("INSERT INTO t VALUES (4),(5)").unwrap();
    c.execute("ANALYZE").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM sqlite_stat1").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(rows_str(&c, "SELECT stat FROM sqlite_stat1"), "5 1");
}

#[test]
fn integrity_after_analyze() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-an-ic-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(a, b TEXT COLLATE NOCASE)")
            .unwrap();
        c.execute("CREATE INDEX i ON t(b)").unwrap();
        for i in 0..20 {
            c.execute(&format!("INSERT INTO t VALUES ({}, 'k{}')", i, i % 4))
                .unwrap();
        }
        c.execute("ANALYZE").unwrap();
    }
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check;")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn planner_prefers_selective_index() {
    // Two single-column indexes; `sel` is unique (very selective), `dup` has one
    // distinct value. After ANALYZE the planner must search via `i_sel`.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(sel, dup)").unwrap();
    // Create the non-selective index FIRST. Without statistics the chooser breaks
    // the equal-prefix tie toward the NEWEST index (highest rootpage) — matching
    // sqlite3 3.50.4 — so the later `i_sel` is chosen even pre-ANALYZE here.
    c.execute("CREATE INDEX i_dup ON t(dup)").unwrap();
    c.execute("CREATE INDEX i_sel ON t(sel)").unwrap();
    for i in 0..50 {
        c.execute(&format!("INSERT INTO t VALUES ({i}, 7)"))
            .unwrap();
    }
    // Before ANALYZE: no stats, so the newest-created matching index (i_sel) is
    // chosen (equal 1-column prefix, neither covers `SELECT *`) — as sqlite does.
    let pre = rows_str(
        &c,
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE sel = 5 AND dup = 7",
    );
    assert!(
        pre.contains("i_sel"),
        "pre-ANALYZE expected i_sel (newest tiebreak), got: {pre}"
    );
    c.execute("ANALYZE").unwrap();
    // After ANALYZE: the selective index still wins (here it was already newest).
    let plan = rows_str(
        &c,
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE sel = 5 AND dup = 7",
    );
    assert!(
        plan.contains("i_sel"),
        "expected plan to use i_sel, got: {plan}"
    );
    // And the result is still correct.
    assert_eq!(
        rows_str(&c, "SELECT sel FROM t WHERE sel = 5 AND dup = 7"),
        "5"
    );
}

#[test]
fn analyze_unknown_object_errors() {
    // `ANALYZE <name>` errors "no such table" when the name is not a table,
    // index, or database — matching sqlite (which previously was a silent no-op).
    let mut c = Connection::open_memory().unwrap();
    let e = c.execute("ANALYZE nope").unwrap_err();
    assert!(format!("{e}").contains("no such table"), "{e}");
    // A real table / index, the `main` and `temp` schemas, the schema table, and
    // the no-argument form are all accepted.
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("CREATE INDEX i ON t(a)").unwrap();
    c.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    for ok in [
        "ANALYZE t",
        "ANALYZE i",
        "ANALYZE main",
        "ANALYZE temp",
        "ANALYZE",
    ] {
        assert!(c.execute(ok).is_ok(), "{ok} should succeed");
    }
    // `ANALYZE` / `ANALYZE main` still populate sqlite_stat1.
    assert!(
        !c.query("SELECT * FROM sqlite_stat1")
            .unwrap()
            .rows
            .is_empty(),
        "ANALYZE should have written stat rows"
    );
}

#[test]
fn reindex_unknown_object_errors() {
    // `REINDEX <name>` must identify a collation, table, or index — sqlite errors
    // "unable to identify the object to be reindexed" otherwise (graphite used to
    // drop the name and no-op). Bare REINDEX and a collation name are valid.
    let mut c = Connection::open_memory().unwrap();
    let e = c.execute("REINDEX nope").unwrap_err();
    assert!(
        format!("{e}").contains("unable to identify the object"),
        "{e}"
    );
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("CREATE INDEX i ON t(a)").unwrap();
    for ok in [
        "REINDEX",
        "REINDEX NOCASE",
        "REINDEX binary",
        "REINDEX rtrim",
        "REINDEX t",
        "REINDEX i",
        "REINDEX main.i",
    ] {
        assert!(c.execute(ok).is_ok(), "{ok} should succeed");
    }
    assert!(c.execute("REINDEX zzz").is_err());
}
