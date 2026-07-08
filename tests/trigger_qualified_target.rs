//! A trigger body's `INSERT`/`UPDATE`/`DELETE` may not schema-qualify its target.
//!
//! SQLite compiles a trigger program in the trigger's own database, so a
//! `schema.table` qualifier on a DML *target* inside the body is rejected at
//! `CREATE TRIGGER` time with `qualified table names are not allowed on INSERT,
//! UPDATE, and DELETE statements within triggers` — regardless of whether the
//! qualified schema/table exists, for main and temp triggers, and for AFTER and
//! INSTEAD OF alike. A qualified table in a *subquery* inside the body (e.g.
//! `SELECT … FROM main.u`) is fine — only the DML target is restricted. graphite
//! used to silently accept the qualified target. The trigger's own missing-table /
//! timing-mismatch / duplicate-name errors still take precedence.
#![cfg(feature = "std")]

use std::process::Command;

fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(sql)
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if !line.is_empty() {
            return line.to_string();
        }
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr.lines() {
        if line.starts_with('^') {
            continue;
        }
        let s = line
            .strip_prefix("Error: in prepare, ")
            .or_else(|| line.strip_prefix("Error: stepping, "))
            .or_else(|| line.strip_prefix("Error: SQL error: "))
            .or_else(|| line.strip_prefix("Error: "))
            .unwrap_or(line);
        let s = s.strip_prefix("error: ").unwrap_or(s);
        let s = s.rsplit_once(" (").map_or(s, |(head, tail)| {
            if tail
                .trim_end_matches(')')
                .chars()
                .all(|c| c.is_ascii_digit())
            {
                head
            } else {
                s
            }
        });
        return s.to_string();
    }
    String::new()
}

fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn same(sql: &str) {
    let g = run(env!("CARGO_BIN_EXE_graphitesql"), sql);
    let s = run("sqlite3", sql);
    assert_eq!(g, s, "mismatch for SQL: {sql}");
}

#[test]
fn trigger_qualified_target_parity() {
    if !sqlite3_available() {
        return;
    }

    // The three DML statement kinds, qualified target -> rejected at CREATE.
    same(
        "CREATE TABLE t(a); CREATE TABLE u(b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO main.u VALUES(1); END;",
    );
    same(
        "CREATE TABLE t(a); CREATE TABLE u(b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE main.u SET b=1; END;",
    );
    same(
        "CREATE TABLE t(a); CREATE TABLE u(b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN DELETE FROM main.u WHERE b=1; END;",
    );

    // Fires regardless of whether the qualified schema/table exists.
    same(
        "CREATE TABLE t(a); CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO nope.u VALUES(1); END;",
    );
    same(
        "CREATE TABLE t(a); CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO main.nope VALUES(1); END;",
    );

    // Temp trigger and INSTEAD OF trigger on a view, too.
    same(
        "CREATE TABLE t(a); CREATE TABLE u(b); CREATE TEMP TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO main.u VALUES(1); END;",
    );
    same(
        "CREATE TABLE t(a); CREATE VIEW v AS SELECT * FROM t; CREATE TABLE u(b); CREATE TRIGGER tr INSTEAD OF INSERT ON v BEGIN INSERT INTO main.u VALUES(1); END;",
    );

    // A later body statement with the qualifier is still caught.
    same(
        "CREATE TABLE t(a); CREATE TABLE u(b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO u VALUES(1); DELETE FROM main.u WHERE b=1; END;",
    );

    // Precedence: the trigger's own errors win over the body check.
    same("CREATE TRIGGER tr AFTER INSERT ON nope BEGIN INSERT INTO main.u VALUES(1); END;");
    same(
        "CREATE TABLE t(a); CREATE TABLE u(b); CREATE TRIGGER tr INSTEAD OF INSERT ON t BEGIN INSERT INTO main.u VALUES(1); END;",
    );
    same(
        "CREATE TABLE t(a); CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END; CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO main.u VALUES(1); END;",
    );

    // A qualified table in a body SUBQUERY (not a DML target) is allowed and runs.
    same(
        "CREATE TABLE t(a); CREATE TABLE u(b); INSERT INTO u VALUES(5); CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO u SELECT b FROM main.u; END; INSERT INTO t VALUES(1); SELECT b FROM u ORDER BY b;",
    );
    same(
        "CREATE TABLE t(a); CREATE TABLE u(b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE u SET b=1 WHERE b=(SELECT max(b) FROM main.u); END; INSERT INTO t VALUES(1);",
    );

    // A plain unqualified trigger still fires correctly.
    same(
        "CREATE TABLE t(a); CREATE TABLE u(b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO u VALUES(NEW.a); END; INSERT INTO t VALUES(7); SELECT b FROM u;",
    );
}
