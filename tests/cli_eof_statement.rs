//! The `graphitesql` shell, reading piped/scripted input, must match the sqlite3
//! shell on two end-of-input details:
//!
//! 1. A final statement not terminated by `;` still runs — SQLite completes the
//!    accumulated buffer at end-of-input, so `SELECT 1` (no trailing `;`) prints
//!    `1`, and `SELECT 1;\nSELECT 2` runs both. graphite used to drop the
//!    unterminated trailing statement.
//! 2. A chunk that holds only comments/whitespace (a trailing comment, or a
//!    `/* c */` between two `;`) is silently ignored rather than raising an
//!    `empty statement` error, so `SELECT 1; /* c */; SELECT 2;` prints `1` then
//!    `2`.
//!
//! Both streams (stdout and stderr) are compared against sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::io::Write;
use std::process::{Command, Stdio};

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Feed `script` on stdin (piped, non-interactive) and return trimmed
/// `(stdout, stderr)` captured separately.
fn piped(bin: &str, script: &str) -> (String, String) {
    let mut child = Command::new(bin)
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&o.stdout).trim_end().to_string(),
        String::from_utf8_lossy(&o.stderr).trim_end().to_string(),
    )
}

#[test]
fn eof_and_comment_statements_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let scripts = [
        // A trailing statement with no terminating `;` still runs at EOF.
        "SELECT 42",
        "SELECT 42\n",
        "SELECT 1;\nSELECT 2",
        "CREATE TABLE t(a);\nINSERT INTO t VALUES(1);\nSELECT * FROM t",
        // Comment-/whitespace-only remainders and chunks are ignored, not errors.
        "-- only a comment\n",
        "/* block */\n",
        "SELECT 1;\n-- trailing comment",
        "SELECT 1; /* c */; SELECT 2;",
        "SELECT 1; /* trailing block only */",
        "/* lead */ SELECT 7",
        "SELECT 1;;SELECT 2;",
        "   \n",
        "",
        // A genuinely incomplete trailing statement still errors at EOF (parity).
        "SELECT (1",
        "SELECT 1;\nSELECT (2",
        // Regression guards: normal terminated scripts are unaffected.
        "SELECT 1;\nSELECT 2;\n",
        "CREATE TABLE t(a,b);\nINSERT INTO t VALUES(1,'x');\nSELECT * FROM t;\n",
    ];
    for s in scripts {
        assert_eq!(piped("sqlite3", s), piped(g, s), "for script {s:?}");
    }
}

/// The same two behaviors hold for statements passed as a one-shot `arg`, which
/// shares the shell's `run_sql_batch` path: a `/* c */` between statements is
/// ignored rather than an `empty statement` error.
#[test]
fn arg_mode_ignores_comment_only_chunks() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let out = |bin: &str, sql: &str| -> String {
        let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        s.trim_end().to_string()
    };
    for sql in [
        "SELECT 1; /* c */; SELECT 2;",
        "SELECT 1;;SELECT 2;",
        "/* only */",
        "SELECT 42",
    ] {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
