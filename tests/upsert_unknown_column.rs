//! An `INSERT … ON CONFLICT … DO …` (UPSERT) clause may reference only the
//! target table's columns — in the conflict target, the target `WHERE`, the
//! `DO UPDATE` assignments and their `WHERE` — plus the `excluded` pseudo-table
//! inside a `DO UPDATE`. SQLite rejects an unknown column at prepare time with
//! `no such column: …` (reported qualified when written `q.c`), in a fixed
//! resolution order; graphite previously ignored the bad reference and ran the
//! statement. Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn unknown_conflict_target_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a PRIMARY KEY, b)").unwrap();
    for sql in [
        "INSERT INTO t VALUES(1,2) ON CONFLICT(x) DO NOTHING",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a,x) DO NOTHING",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(x) DO UPDATE SET b=1",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) WHERE x>0 DO NOTHING",
    ] {
        assert!(
            c.execute(sql)
                .unwrap_err()
                .to_string()
                .contains("no such column: x"),
            "for {sql}"
        );
    }
    // A real column (and the rowid) remain valid targets.
    c.execute("INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO NOTHING")
        .unwrap();
    c.execute("INSERT INTO t VALUES(3,4) ON CONFLICT(rowid) DO NOTHING")
        .unwrap();
}

#[test]
fn unknown_do_update_column_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a PRIMARY KEY, b)").unwrap();
    // Unknown assigned (target) column.
    assert!(c
        .execute("INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET z=1")
        .unwrap_err()
        .to_string()
        .contains("no such column: z"));
    // Unknown column in an assignment value.
    assert!(c
        .execute("INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET b=zz")
        .unwrap_err()
        .to_string()
        .contains("no such column: zz"));
    // Unknown column behind the `excluded.` qualifier, reported qualified.
    assert!(c
        .execute("INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET b=excluded.nope")
        .unwrap_err()
        .to_string()
        .contains("no such column: excluded.nope"));
    // Unknown column in the update `WHERE`.
    assert!(c
        .execute("INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET b=1 WHERE qq>0")
        .unwrap_err()
        .to_string()
        .contains("no such column: qq"));
}

#[test]
fn valid_upsert_still_runs() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a PRIMARY KEY, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,2)").unwrap();
    // `excluded.<col>`, a bare table column, `table.col`, and the rowid all resolve.
    c.execute(
        "INSERT INTO t VALUES(1,9) ON CONFLICT(a) \
         DO UPDATE SET b=excluded.b WHERE t.a>0",
    )
    .unwrap();
    let rows = c.query("SELECT b FROM t WHERE a=1").unwrap();
    assert_eq!(rows.rows[0][0], graphitesql::Value::Integer(9));
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string()
    };
    let b = "CREATE TABLE t(a PRIMARY KEY, b);";
    for tail in [
        "INSERT INTO t VALUES(1,2) ON CONFLICT(x) DO NOTHING",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a,x) DO NOTHING",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(rowid) DO NOTHING",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET z=1",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET b=zz",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET b=excluded.nope",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET b=excluded.b",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET b=t.nope",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET b=1 WHERE qq>0",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(x) DO UPDATE SET b=1",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) WHERE x>0 DO NOTHING",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) WHERE excluded.b>0 DO NOTHING",
        // ordering: RHS before SET target; conflict target before SET target
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET z=zz",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(x) DO UPDATE SET b=zz",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) WHERE qq>0 DO UPDATE SET b=zz",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET b=zz WHERE ww>0",
        "INSERT INTO t VALUES(1,2) ON CONFLICT(a) DO UPDATE SET z=1 WHERE ww>0",
    ] {
        let sql = format!("{b} {tail}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {sql}");
    }
}
