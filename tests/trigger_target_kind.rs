//! Trigger-target validation and the view-modification message.
//!
//! `INSTEAD OF` triggers may attach only to a view; `BEFORE`/`AFTER` triggers
//! only to a real table. sqlite rejects the mismatch at CREATE with
//! `cannot create INSTEAD OF trigger on table: NAME` /
//! `cannot create BEFORE|AFTER trigger on view: NAME`. graphite previously
//! created the trigger silently. Separately, a direct INSERT/UPDATE/DELETE on a
//! view (with no INSTEAD OF trigger) now reports sqlite's exact wording,
//! `cannot modify NAME because it is a view` (was `… — it is a view`). Matched
//! to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn trigger_target_kind_is_enforced() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE tbl(a)").unwrap();
    c.execute("CREATE VIEW vw AS SELECT 1 a").unwrap();

    let err = |c: &mut Connection, sql: &str| c.execute(sql).unwrap_err().to_string();

    assert!(
        err(
            &mut c,
            "CREATE TRIGGER t1 INSTEAD OF INSERT ON tbl BEGIN SELECT 1; END"
        )
        .contains("cannot create INSTEAD OF trigger on table: tbl")
    );
    assert!(
        err(
            &mut c,
            "CREATE TRIGGER t2 BEFORE INSERT ON vw BEGIN SELECT 1; END"
        )
        .contains("cannot create BEFORE trigger on view: vw")
    );
    assert!(
        err(
            &mut c,
            "CREATE TRIGGER t3 AFTER INSERT ON vw BEGIN SELECT 1; END"
        )
        .contains("cannot create AFTER trigger on view: vw")
    );

    // The valid combinations still succeed.
    c.execute("CREATE TRIGGER ok1 INSTEAD OF INSERT ON vw BEGIN SELECT 1; END")
        .unwrap();
    c.execute("CREATE TRIGGER ok2 BEFORE INSERT ON tbl BEGIN SELECT 1; END")
        .unwrap();
}

#[test]
fn modifying_a_view_uses_sqlite_wording() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIEW vw AS SELECT 1 a").unwrap();
    for sql in [
        "INSERT INTO vw VALUES(1)",
        "UPDATE vw SET a=1",
        "DELETE FROM vw",
    ] {
        assert_eq!(
            c.execute(sql).unwrap_err().to_string(),
            "error: cannot modify vw because it is a view",
            "for {sql}"
        );
    }
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
        let err = String::from_utf8_lossy(&out.stderr);
        err.lines()
            .next()
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string()
    };
    let base = "CREATE TABLE tbl(a);CREATE VIEW vw AS SELECT 1 a;";
    for spec in [
        "CREATE TRIGGER t1 INSTEAD OF INSERT ON tbl BEGIN SELECT 1; END",
        "CREATE TRIGGER t2 BEFORE INSERT ON vw BEGIN SELECT 1; END",
        "CREATE TRIGGER t3 AFTER DELETE ON vw BEGIN SELECT 1; END",
        "INSERT INTO vw VALUES(1)",
        "UPDATE vw SET a=1",
        "DELETE FROM vw",
    ] {
        let full = format!("{base}{spec}");
        assert_eq!(run("sqlite3", &full), run(g, &full), "for {spec}");
    }
}
