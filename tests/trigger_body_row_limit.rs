//! A `CREATE TRIGGER` body's `UPDATE`/`DELETE` may not use the `ORDER BY` /
//! `LIMIT` row-limit extension — SQLite's trigger-step grammar has no room for
//! it, so a body `UPDATE … ORDER BY` / `… LIMIT` is a parse-time
//! `near "ORDER"`/`near "LIMIT": syntax error`. graphite used to parse it and
//! silently no-op. The same clauses stay legal on a *top-level* UPDATE/DELETE
//! and inside a body subquery's SELECT. Verified against sqlite3 3.50.4.
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

/// Whether the local `sqlite3` has the `UPDATE/DELETE … ORDER BY … LIMIT`
/// extension (`SQLITE_ENABLE_UPDATE_DELETE_LIMIT`). The *body* cases here are a
/// syntax error with or without it (triggers always forbid the clause), but the
/// top-level negative controls execute only on a build that has it — and CI's
/// pinned stock 3.50.4 does not. Probe with a valid statement.
fn sqlite3_has_update_delete_limit() -> bool {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg("CREATE TABLE t(a); INSERT INTO t VALUES(1); UPDATE t SET a=1 ORDER BY a LIMIT 1;")
        .output();
    match out {
        Ok(o) => !String::from_utf8_lossy(&o.stderr).contains("syntax error"),
        Err(_) => false,
    }
}

#[test]
fn trigger_body_row_limit_parity() {
    if !sqlite3_available() {
        return;
    }
    let t = "CREATE TABLE t(a, b);";

    // ORDER BY / LIMIT on a body UPDATE or DELETE -> syntax error, keyword echoed.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a=1 ORDER BY a; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN DELETE FROM t ORDER BY a; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a=1 LIMIT 1; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN DELETE FROM t LIMIT 1; END;"
    ));
    // With a WHERE / OFFSET present, and in lower case (verbatim echo).
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a=1 WHERE a=2 ORDER BY a LIMIT 1; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN DELETE FROM t LIMIT 1 OFFSET 2; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a=1 limit 1; END;"
    ));
    // INSTEAD OF / temp triggers reject it the same way.
    same(
        "CREATE VIEW v AS SELECT 1 a; CREATE TRIGGER tr INSTEAD OF INSERT ON v BEGIN DELETE FROM x ORDER BY 1; END;",
    );

    // Negative controls: the extension still parses at top level — but only
    // against a sqlite3 built with `SQLITE_ENABLE_UPDATE_DELETE_LIMIT` (CI's
    // pinned stock 3.50.4 is not; graphite always supports it).
    if sqlite3_has_update_delete_limit() {
        same(&format!(
            "{t} INSERT INTO t(a) VALUES(1),(2); UPDATE t SET b=9 ORDER BY a LIMIT 1; SELECT count(*) FROM t WHERE b=9;"
        ));
        same(&format!(
            "{t} INSERT INTO t(a) VALUES(1),(2); DELETE FROM t ORDER BY a LIMIT 1; SELECT count(*) FROM t;"
        ));
    }
    // … a plain body UPDATE still builds and fires …
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET b=1 WHERE a=new.a; END; INSERT INTO t(a) VALUES(5); SELECT b FROM t;"
    ));
    // … and ORDER BY inside a body subquery's SELECT stays legal.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET b=(SELECT a FROM t ORDER BY a LIMIT 1) WHERE a=new.a; END; INSERT INTO t(a) VALUES(5); SELECT b FROM t;"
    ));

    // Precedence: the trigger target is resolved before the body steps are
    // parsed, so a missing-table / system-table / timing-mismatch error must
    // outrank the deferred body row-limit syntax error (the parser records it
    // rather than throwing, so these win).
    // Missing target -> `no such table: main.nope`, not `near "ORDER"`.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON nope BEGIN UPDATE t SET a=1 ORDER BY a; END;"
    ));
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON nope BEGIN DELETE FROM t LIMIT 1; END;"
    ));
    // System-table target -> `cannot create trigger on system table`.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON sqlite_master BEGIN UPDATE t SET a=1 ORDER BY a; END;"
    ));
    // Timing mismatch (BEFORE on a view) -> `cannot create BEFORE trigger on view: v`.
    same(&format!(
        "{t} CREATE VIEW v AS SELECT 1 a; CREATE TRIGGER tr BEFORE INSERT ON v BEGIN UPDATE t SET a=1 ORDER BY a; END;"
    ));
    // Duplicate name still outranks everything (existing trigger).
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END; CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a=1 ORDER BY a; END;"
    ));
    // A qualified body DML target (also a body-grammar error) is reported in
    // preference to a same-statement row-limit, matching SQLite.
    same(&format!(
        "{t} CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE main.t SET a=1 ORDER BY a; END;"
    ));
}
