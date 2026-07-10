//! The `graphitesql` shell renders one-shot (`-arg`) errors the way the sqlite3
//! shell does: `Error: in prepare, <msg>` with a `^--- error here` source-line
//! caret for a compile-time error (when the message names a locatable token), and
//! `Error: stepping, <msg> [(<code>)]` for a run-time error. Previously graphite
//! printed a bare `Error: <msg>` with graphite's own `error:` / `SQL error:` tag.
//!
//! The offending-token caret is located by a text search of the failed statement,
//! which is byte-exact with SQLite for the common cases (an identifier that appears
//! once in code); a couple of edge cases — a repeated operator token (`===`) and a
//! very long statement that SQLite windows — are not reproduced and are omitted.
//!
//! Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s.trim_end().to_string()
}

#[test]
fn one_shot_error_rendering_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Prepare-time errors with a caret.
        "SELCT 1",
        "SELECT froed",
        "CREATE TABLE t(a); SELECT t.zzz FROM t",
        "SELECT bogusfn(1)",
        "SELECT abs()",
        "CREATE TABLE a(x); CREATE TABLE b(x); SELECT x FROM a,b",
        "CREATE TABLE t(a); SELECT max(min(a)) FROM t",
        "SELECT \"x\" FROM (SELECT 1)",
        "SELECT 0o5",
        "SELECT 'abc",
        "CREATE TABLE t(a); CREATE TABLE t(a)",
        "CREATE INDEX i ON t2(a); CREATE TABLE t2(a); CREATE INDEX i ON t2(a)",
        // Prepare-time errors without a caret (no source position).
        "SELECT * FROM nope",
        "INSERT INTO nope VALUES(1)",
        "DROP TABLE nope",
        "DROP INDEX nope",
        "CREATE TABLE t(a); SELECT a FROM t ORDER BY a COLLATE nope",
        "CREATE TABLE t(a,b); INSERT INTO t VALUES(1)",
        "CREATE TABLE t(a,a)",
        "CREATE TABLE t(a); ALTER TABLE t ADD COLUMN a",
        "SELECT (SELECT 1,2)",
        "CREATE TABLE t(a); SELECT a FROM t GROUP BY count(*)",
        "CREATE TABLE t(a PRIMARY KEY, b PRIMARY KEY)",
        "SELECT count(*) OVER ()",
        "SELECT",
        // Step-time errors (constraints carry the (19) code; a plain error does not).
        "CREATE TABLE t(a UNIQUE); INSERT INTO t VALUES(1),(1)",
        "CREATE TABLE t(a NOT NULL); INSERT INTO t VALUES(NULL)",
        "CREATE TABLE t(a CHECK(a>0)); INSERT INTO t VALUES(-1)",
        "CREATE TABLE t(a INT) STRICT; INSERT INTO t VALUES('x')",
        "SELECT json_extract('bad','$')",
        "CREATE TABLE t(a); CREATE TRIGGER x BEFORE INSERT ON t BEGIN SELECT RAISE(ABORT,'no'); END; \
         INSERT INTO t VALUES(1)",
        "PRAGMA foreign_keys=ON; CREATE TABLE p(i PRIMARY KEY); CREATE TABLE c(x REFERENCES p); \
         INSERT INTO c VALUES(9)",
        // A token that also appears inside a string is not mistaken for the code ref.
        "SELECT 'froed', froed",
        "SELECT 'bogusfn', bogusfn(1)",
        // A deep resolution error uses the left-decorated caret.
        "CREATE TABLE t(a); SELECT a, aa, bb, cc, dd, badcol FROM t",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
