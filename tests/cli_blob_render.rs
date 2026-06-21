//! In list mode the `graphitesql` shell prints a BLOB (and a TEXT value with an
//! embedded NUL) as its raw bytes up to the first NUL — exactly as the sqlite3
//! CLI does, which hands the value to C string routines. `SELECT x'48656c6c6f'`
//! prints `Hello` (not `48656c6c6f`); `x'410042'` and `'A'||char(0)||'B'` print
//! `A`. Previously the shell rendered blobs as hex digits.

#![cfg(feature = "std")]

use std::process::Command;

/// Run a one-shot query and return raw stdout bytes (blobs are not valid UTF-8).
fn run_bytes(sql: &str) -> Vec<u8> {
    let out = Command::new(env!("CARGO_BIN_EXE_graphitesql"))
        .arg(":memory:")
        .arg(sql)
        .output()
        .expect("run graphitesql shell");
    assert!(out.status.success(), "query failed: {sql}");
    out.stdout
}

#[test]
fn blob_prints_raw_bytes_to_first_nul() {
    // "Hello" as a blob prints as text, not hex.
    assert_eq!(run_bytes("SELECT x'48656c6c6f'"), b"Hello\n");
    // Truncated at the first NUL.
    assert_eq!(run_bytes("SELECT x'410042'"), b"A\n");
    // A blob that is all/leading NUL prints nothing (just the newline).
    assert_eq!(run_bytes("SELECT zeroblob(3)"), b"\n");
    assert_eq!(run_bytes("SELECT x'00ff00'"), b"\n");
}

#[test]
fn text_with_embedded_nul_also_truncates() {
    assert_eq!(run_bytes("SELECT 'A'||char(0)||'B'"), b"A\n");
    // Ordinary text is unaffected.
    assert_eq!(run_bytes("SELECT 'plain text'"), b"plain text\n");
}

#[test]
fn mixed_columns_keep_the_pipe_separator() {
    // A row mixing text, blob, and integer still joins on '|'.
    assert_eq!(run_bytes("SELECT 'x', x'4869', 42"), b"x|Hi|42\n");
}
