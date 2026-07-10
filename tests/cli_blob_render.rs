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

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite3_bytes(sql: &str) -> Vec<u8> {
    Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .expect("run sqlite3")
        .stdout
}

/// List/tabs mode applies SQLite's default control-character display escaping
/// (`SHELL_ESC_ASCII`): a control byte `c ≤ 0x1f` other than tab/newline/CRLF-CR
/// prints as `^` + `c+0x40`. `ascii` mode sends bytes verbatim.
#[test]
fn list_mode_escapes_control_chars_like_sqlite() {
    assert_eq!(run_bytes("SELECT x'02', x'01'"), b"^B|^A\n");
    // A lone CR is escaped (^M); a CRLF pair is preserved verbatim.
    assert_eq!(run_bytes("SELECT char(65,13,66)"), b"A^MB\n");
    assert_eq!(run_bytes("SELECT char(13,10)"), b"\r\n\n");
    // ESC → ^[; tab (0x09) and DEL (0x7f) pass through raw.
    assert_eq!(run_bytes("SELECT x'1b', x'09', x'7f'"), b"^[|\t|\x7f\n");
    // Ordinary text is untouched.
    assert_eq!(run_bytes("SELECT 'hi', 5"), b"hi|5\n");

    if sqlite3_available() {
        for sql in [
            "SELECT x'02', x'01'",
            "SELECT char(65,13,66)",
            "SELECT char(13,10)",
            "SELECT x'1b1c1d1e1f'",
            "SELECT x'48656c6c6f'",
        ] {
            assert_eq!(run_bytes(sql), sqlite3_bytes(sql), "vs sqlite3 for {sql}");
        }
    }
}

/// `ascii` mode does not escape (it uses the unit/record separators for machine
/// parsing) — the raw control byte survives. Fed on stdin, since dot-commands
/// are not parsed from a single one-shot SQL argument.
#[test]
fn ascii_mode_does_not_escape() {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_graphitesql"))
        .arg(":memory:")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b".mode ascii\nSELECT x'02';")
        .unwrap();
    let out = child.wait_with_output().unwrap().stdout;
    assert_eq!(out, b"\x02\x1e");
}
