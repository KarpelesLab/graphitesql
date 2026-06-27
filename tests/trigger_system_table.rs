//! `CREATE TRIGGER` refuses a system table as its target, matching SQLite's
//! `cannot create trigger on system table` and its precedence.
//!
//! The schema tables (`sqlite_master` / `sqlite_schema` / `sqlite_temp_master`)
//! always count as system tables; any other `sqlite_`-prefixed table counts only
//! when it physically exists (e.g. `sqlite_sequence` after an AUTOINCREMENT
//! table is created). The check outranks the missing-table, timing-mismatch and
//! body-qualifier errors, but is itself outranked by the duplicate-name error.
//! graphite used to report `no such table` for the schema tables and even
//! succeeded on an existing `sqlite_sequence`. Verified against sqlite3 3.50.4.
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
fn trigger_system_table_parity() {
    if !sqlite3_available() {
        return;
    }

    // The schema tables are always system tables, in any letter case and for any
    // timing/temp variant.
    same("CREATE TRIGGER tr AFTER INSERT ON sqlite_master BEGIN SELECT 1; END;");
    same("CREATE TRIGGER tr AFTER INSERT ON sqlite_schema BEGIN SELECT 1; END;");
    same("CREATE TRIGGER tr AFTER INSERT ON SQLITE_MASTER BEGIN SELECT 1; END;");
    same("CREATE TRIGGER tr BEFORE INSERT ON sqlite_master BEGIN SELECT 1; END;");
    same("CREATE TRIGGER tr INSTEAD OF INSERT ON sqlite_master BEGIN SELECT 1; END;");
    same("CREATE TEMP TRIGGER tr AFTER INSERT ON sqlite_master BEGIN SELECT 1; END;");
    same("CREATE TRIGGER IF NOT EXISTS tr AFTER INSERT ON sqlite_master BEGIN SELECT 1; END;");

    // A body qualifier does not pre-empt the system-table error.
    same("CREATE TABLE u(b); CREATE TRIGGER tr AFTER INSERT ON sqlite_master BEGIN INSERT INTO main.u VALUES(1); END;");

    // A non-schema `sqlite_` table is a system table only when it exists.
    same("CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT); CREATE TRIGGER tr AFTER INSERT ON sqlite_sequence BEGIN SELECT 1; END;");
    same("CREATE TRIGGER tr AFTER INSERT ON sqlite_sequence BEGIN SELECT 1; END;");
    same("CREATE TRIGGER tr AFTER INSERT ON sqlite_foo BEGIN SELECT 1; END;");

    // The duplicate-name error still outranks the system-table error.
    same("CREATE TABLE x(a); CREATE TRIGGER tr AFTER INSERT ON x BEGIN SELECT 1; END; CREATE TRIGGER tr AFTER INSERT ON sqlite_master BEGIN SELECT 1; END;");

    // Regression: a trigger on an ordinary table still builds and fires.
    same("CREATE TABLE t(a, b); CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET b=1; END; INSERT INTO t(a) VALUES(7); SELECT b FROM t;");
}
