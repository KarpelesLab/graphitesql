//! `sha1(X)` and `sha3(X[, size])` hash functions, ported from sqlite's
//! `ext/misc/sha1.c` and `ext/misc/shathree.c`. `sha1()` returns the digest as a
//! 40-byte lower-case-hex BLOB (matching sqlite, whose binary variant is the
//! separate `sha1b`); `sha3()` returns the raw `size/8`-byte digest. Verified
//! byte-for-byte against the sqlite3 3.50.4 CLI, which has both compiled in.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Known-answer vectors that hold regardless of whether the sqlite3 CLI is
/// installed. Uses `hex()`/`quote()` so the whole comparison is ASCII text.
#[test]
fn sha_known_vectors() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // sha1() renders lower-case hex into a 40-byte blob.
        (
            "SELECT hex(sha1('abc'))",
            // hex of the ascii text "a9993e36...d89d"
            "61393939336533363437303638313661626133653235373137383530633236633963643064383964",
        ),
        (
            "SELECT sha1('abc')",
            "a9993e364706816aba3e25717850c26c9cd0d89d",
        ),
        (
            "SELECT sha1('')",
            "da39a3ee5e6b4b0d3255bfef95601890afd80709",
        ),
        ("SELECT quote(sha1(NULL))", "NULL"),
        (
            "SELECT hex(sha3(''))",
            "A7FFC6F8BF1ED76651C14756A061D662F580FF4DE43B49FA82D80A4B80F8434A",
        ),
        (
            "SELECT hex(sha3('abc'))",
            "3A985DA74FE225B2045C172D6BD390BD855F086E3E9D525B46BFE24511431532",
        ),
        (
            "SELECT hex(sha3('abc',224))",
            "E642824C3F8CF24AD09234EE7D3C766FC9A3A5168D0C94AD73B46FDF",
        ),
        (
            "SELECT hex(sha3('abc',512))",
            "B751850B1A57168A5693CD924B6B096E08F621827444F70D884F5D0240D2712E\
             10E116E9192AF3C91A7EC57647E3934057340B4CF408D5A56592F8274EEC53F0",
        ),
        ("SELECT quote(sha3(NULL))", "NULL"),
        // An integer/real hashes its text rendering: sha3(1) == sha3('1').
        ("SELECT sha3(1) = sha3('1')", "1"),
    ];
    for (sql, want) in cases {
        assert_eq!(out(g, sql).trim_end(), want, "sql: {sql}");
    }
}

/// Differential comparison against the real sqlite3 CLI over a range of argument
/// types, sizes, and block boundaries.
#[test]
fn sha_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping differential check");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");

    let a55: String = "a".repeat(55);
    let a56: String = "a".repeat(56);
    let a64: String = "a".repeat(64);
    let b135: String = "b".repeat(135); // rate-1 for SHA3-256 (special 0x86 pad)
    let b136: String = "b".repeat(136); // exactly one SHA3-256 rate block
    let big: String = "x".repeat(200); // spans multiple blocks

    let mut sql = String::new();
    for arg in [
        "'abc'",
        "''",
        "'1'",
        "1",
        "456",
        "1.5",
        "-0.0",
        "9223372036854775807",
        "x''",
        "x'00ff01'",
        "NULL",
    ] {
        for size in ["", ",224", ",256", ",384", ",512"] {
            sql.push_str(&format!("SELECT hex(sha3({arg}{size}));"));
        }
        sql.push_str(&format!("SELECT hex(sha1({arg}));"));
    }
    for s in [&a55, &a56, &a64, &b135, &b136, &big] {
        sql.push_str(&format!("SELECT hex(sha1('{s}'));"));
        sql.push_str(&format!("SELECT hex(sha3('{s}'));"));
        sql.push_str(&format!("SELECT hex(sha3('{s}',224));"));
        sql.push_str(&format!("SELECT hex(sha3('{s}',512));"));
    }

    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}

/// The bad-`size` error message matches sqlite exactly, and is raised even for a
/// NULL argument (sqlite validates the size before the NULL short-circuit).
#[test]
fn sha3_bad_size_error() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        "SELECT sha3('abc',999)",
        "SELECT sha3(NULL,999)",
        "SELECT sha3('x',0)",
    ] {
        let o = Command::new(g).arg(":memory:").arg(sql).output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            err.contains("SHA3 size should be one of: 224 256 384 512"),
            "sql {sql} gave stderr: {err}"
        );
    }
}
