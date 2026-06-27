//! The `sql` column of `sqlite_schema` (`sqlite_master`) stores a *canonicalised*
//! form of each `CREATE` statement, not the verbatim input: SQLite regenerates
//! the `CREATE <TYPE> ` head — dropping `IF NOT EXISTS` and any `TEMP`, and
//! collapsing its whitespace to single spaces — then appends the source text from
//! the object-name token onward with the trailing `;` (and surrounding
//! whitespace) removed. graphite previously stored the statement verbatim,
//! including the trailing semicolon and `IF NOT EXISTS`, so every schema row's
//! `sql` diverged. Verified against the sqlite3 3.50.4 CLI.
//!
//! `TEMP` creates store the same canonical form (the `TEMP` modifier is
//! dropped), schema-qualified creates (`CREATE TABLE aux.t …`) drop the
//! `schema.` prefix, and `CREATE TABLE … AS SELECT` writes its column list with
//! SQLite's `identPut` quoting (bare when safe, no spaces after commas) — all
//! verified against the sqlite3 CLI below.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> String {
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
        .trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .trim_end()
        .to_string()
}

#[test]
fn stored_sql_drops_trailing_semicolon_and_if_not_exists() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    assert_eq!(
        run(g, "CREATE TABLE t(a,b); SELECT sql FROM sqlite_master"),
        "CREATE TABLE t(a,b)"
    );
    assert_eq!(
        run(
            g,
            "CREATE TABLE IF NOT EXISTS t(a,b); SELECT sql FROM sqlite_master"
        ),
        "CREATE TABLE t(a,b)"
    );
    // The prefix whitespace is normalised but the body is kept verbatim.
    assert_eq!(
        run(g, "CREATE   TABLE t(a,   b); SELECT sql FROM sqlite_master"),
        "CREATE TABLE t(a,   b)"
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        "CREATE TABLE t(a,b); SELECT sql FROM sqlite_master",
        "CREATE TABLE IF NOT EXISTS t(a,b); SELECT sql FROM sqlite_master",
        "CREATE TABLE t(a,b)   ;   SELECT sql FROM sqlite_master",
        "  CREATE   TABLE t(a,   b)  ; SELECT sql FROM sqlite_master",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT NOT NULL); SELECT sql FROM sqlite_master",
        "CREATE TABLE \"my tbl\"(a); SELECT sql FROM sqlite_master",
        // Index: UNIQUE is kept, IF NOT EXISTS dropped.
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(b); SELECT sql FROM sqlite_master WHERE type='index'",
        "CREATE TABLE t(a,b); CREATE UNIQUE INDEX IF NOT EXISTS i ON t(a); SELECT sql FROM sqlite_master WHERE type='index'",
        // View and trigger (the trigger body's internal `;` is preserved).
        "CREATE TABLE t(a); CREATE VIEW IF NOT EXISTS v AS SELECT a+1 FROM t; SELECT sql FROM sqlite_master WHERE type='view'",
        "CREATE TABLE t(a); CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END; SELECT sql FROM sqlite_master WHERE type='trigger'",
        // TEMP creates store the canonical (TEMP-stripped) head.
        "CREATE TEMP TABLE t(a,b); SELECT sql FROM sqlite_temp_master",
        "CREATE TEMPORARY TABLE t(a,   b); SELECT sql FROM sqlite_temp_master",
        // CREATE TABLE … AS SELECT: identPut quoting, no spaces after commas.
        "CREATE TABLE s(a,c); CREATE TABLE t AS SELECT a, c FROM s; SELECT sql FROM sqlite_master WHERE name='t'",
        "CREATE TABLE s(a,c); CREATE TABLE t AS SELECT a+1 AS x, c FROM s; SELECT sql FROM sqlite_master WHERE name='t'",
        "CREATE TABLE s(a); CREATE TABLE t AS SELECT a AS \"x y\" FROM s; SELECT sql FROM sqlite_master WHERE name='t'",
        // A column named like a keyword is quoted; a plain mixed-case one is not.
        "CREATE TABLE s(a); CREATE TABLE t AS SELECT a AS \"select\" FROM s; SELECT sql FROM sqlite_master WHERE name='t'",
        "CREATE TABLE s(a); CREATE TABLE t AS SELECT a AS key FROM s; SELECT sql FROM sqlite_master WHERE name='t'",
        "CREATE TABLE s(a); CREATE TABLE t AS SELECT a AS BigName FROM s; SELECT sql FROM sqlite_master WHERE name='t'",
        // Duplicate output names get SQLite's `:N` suffix (which then needs quoting).
        "CREATE TABLE s(a); CREATE TABLE t AS SELECT a, a FROM s; SELECT sql FROM sqlite_master WHERE name='t'",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}

/// Schema-qualified creates (`CREATE TABLE aux.t …`) store the bare object name
/// in the attached catalog, byte-identical to sqlite.
#[test]
fn schema_qualified_create_drops_prefix() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir();
    for (i, body) in [
        "CREATE TABLE aux.t(a,b)",
        "CREATE TABLE aux.t(a INTEGER PRIMARY KEY, b TEXT)",
        "CREATE TABLE aux.t(a,b); CREATE INDEX aux.i ON t(b)",
        "CREATE TABLE aux.t(a); CREATE VIEW aux.v AS SELECT a FROM t",
    ]
    .iter()
    .enumerate()
    {
        // Distinct attach-file names keep the parallel test threads independent;
        // the file is cleared before each engine so neither sees the other's table.
        let aux = dir.join(format!("graphite_schema_qual_{i}.db"));
        let sql = format!(
            "ATTACH '{}' AS aux; {body}; SELECT sql FROM aux.sqlite_master",
            aux.display()
        );
        let _ = std::fs::remove_file(&aux);
        let want = run("sqlite3", &sql);
        let _ = std::fs::remove_file(&aux);
        let got = run(g, &sql);
        let _ = std::fs::remove_file(&aux);
        assert_eq!(want, got, "for {body}");
    }
}
