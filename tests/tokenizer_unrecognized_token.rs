//! A lexing failure — a stray character, an unterminated literal, a malformed
//! blob, or a number run with a trailing identifier — is reported by SQLite as
//! `unrecognized token: "X"`, where `X` is the verbatim source text from the
//! token's start to where the lexer gave up (for a misplaced `_` digit
//! separator, the text as `sqlite3DequoteNumber` left it mid-rewrite). graphite
//! previously surfaced its own lexer state (`unexpected character '^' at byte
//! 7`, `unterminated string literal at byte 11`, …); every such site now
//! renders the SQLite-compatible message. An unterminated block comment runs
//! to the end of the input and is *whitespace* (tokenize.c CC_SLASH stops its
//! scan at NUL and still yields TK_COMMENT), a leading UTF-8 BOM is skipped,
//! and a `#N` register reference is rejected with sqlite's
//! `near "#N": syntax error`. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The library renders a parse error as `SQL error: <msg>`; return just `<msg>`.
fn parse_msg(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("SQL error: ")
        .to_string()
}

#[test]
fn lexing_failures_report_the_offending_source_run() {
    let c = Connection::open_memory().unwrap();
    // (sql, the verbatim token text SQLite echoes).
    for (sql, tok) in [
        ("SELECT ^", "^"),
        ("SELECT #", "#"),
        ("SELECT @", "@"),
        ("SELECT $", "$"),
        ("SELECT :", ":"),
        ("SELECT 'abc", "'abc"),
        ("SELECT \"abc", "\"abc"),
        ("SELECT x'zz'", "x'zz'"),
        ("SELECT x'1", "x'1"),
        ("SELECT x'1'", "x'1'"),
        ("SELECT 0xGG", "0xGG"),
        ("SELECT 0x", "0x"),
        ("SELECT 1e", "1e"),
        // A dangling exponent stops before the sign: `1e` is the bad token,
        // the `+` lexes separately.
        ("SELECT 1e+", "1e"),
        ("SELECT 123abc", "123abc"),
        ("SELECT 1.5xyz", "1.5xyz"),
        ("SELECT 12e3f", "12e3f"),
        // `$` is an identifier character (IdChar), so `1$` is one bad token.
        ("SELECT 1$", "1$"),
        // A lone `!` (not `!=`) is TK_ILLEGAL.
        ("SELECT !", "!"),
        // An unterminated `[identifier]` runs to the end of the input.
        ("SELECT [abc", "[abc"),
        // A misplaced `_` digit separator is reported with sqlite's
        // mid-dequote buffer: stripped prefix + untouched remainder.
        ("SELECT 1_", "1_"),
        ("SELECT 1__2", "1__2"),
        ("SELECT 1_2_", "122_"),
        ("SELECT 0x1_2_", "0x122_"),
        ("SELECT 1_.5", "1_.5"),
        // An unterminated `$name(subscript` (TCL-style) is one bad token; the
        // scan stops at whitespace, not at `;`.
        ("SELECT $x( )", "$x("),
        ("SELECT $x(abc;", "$x(abc;"),
    ] {
        assert_eq!(
            parse_msg(&c, sql),
            format!("unrecognized token: \"{tok}\""),
            "for {sql}"
        );
    }
}

#[test]
fn hex_literal_too_big_is_its_own_message() {
    // A hex literal is a *recognized* token even when it overflows 64 bits;
    // SQLite then rejects it with a dedicated `hex literal too big: <literal>`
    // rather than the generic `unrecognized token`. The literal is echoed with
    // case preserved but `_` separators stripped (`sqlite3DequoteNumber`
    // rewrites the token before `codeInteger` reports it).
    let c = Connection::open_memory().unwrap();
    for (sql, lit) in [
        ("SELECT 0x10000000000000000", "0x10000000000000000"),
        ("SELECT 0x1ffffffffffffffff", "0x1ffffffffffffffff"),
        ("SELECT 0xFFFFFFFFFFFFFFFFF", "0xFFFFFFFFFFFFFFFFF"),
        ("SELECT 0x1_0000000000000000", "0x10000000000000000"),
        ("SELECT 0x10000000000000000 + 0", "0x10000000000000000"),
    ] {
        assert_eq!(
            parse_msg(&c, sql),
            format!("hex literal too big: {lit}"),
            "for {sql}"
        );
    }
    // The boundary value (exactly 64 bits) still lexes — it is -1 as i64.
    assert!(c.query("SELECT 0xffffffffffffffff").is_ok());
}

#[test]
fn unterminated_block_comment_is_whitespace() {
    // tokenize.c CC_SLASH scans a `/*` comment to its terminator or the end of
    // the input and yields TK_COMMENT either way: an unterminated block
    // comment is trailing whitespace, not an error. `SELECT /*…` then fails
    // only because the SELECT itself is truncated (`incomplete input`), while
    // `SELECT 1 /*…` is a complete statement.
    let c = Connection::open_memory().unwrap();
    assert_eq!(parse_msg(&c, "SELECT /* unterminated"), "incomplete input");
    let rows = c.query("SELECT 1 /* x").unwrap();
    assert_eq!(rows.rows.len(), 1);
    // A bare `/*` at the very end is a division operator and a star instead
    // (`z[1]!='*' || z[2]==0`), so the error names the `/`.
    assert_eq!(parse_msg(&c, "/*"), "near \"/\": syntax error");
}

#[test]
fn leading_bom_is_skipped() {
    let c = Connection::open_memory().unwrap();
    let rows = c.query("\u{feff}SELECT 42").unwrap();
    assert_eq!(rows.rows.len(), 1);
}

#[test]
fn register_reference_is_a_syntax_error() {
    // `#N` (a `#`-variable whose name starts with a digit) is sqlite's
    // internal register reference, valid only in a nested parse; user SQL gets
    // `near "#N": syntax error` from the `expr ::= VARIABLE` action. An error
    // raised at the very token that triggered that reduction replaces it,
    // exactly as sqlite's parser (which keeps processing that one token).
    let c = Connection::open_memory().unwrap();
    for (sql, msg) in [
        ("SELECT #1", "near \"#1\": syntax error"),
        ("SELECT #1;", "near \"#1\": syntax error"),
        ("SELECT 1+#2;", "near \"#2\": syntax error"),
        (
            "SELECT #999999999999;",
            "near \"#999999999999\": syntax error",
        ),
        ("SELECT #1abc;", "near \"#1abc\": syntax error"),
        ("SELECT #1 FROM sqlite_schema;", "near \"#1\": syntax error"),
        ("SELECT (#1);", "near \"#1\": syntax error"),
        ("SELECT #1 +;", "near \"#1\": syntax error"),
        // The later error at the trigger token wins.
        ("SELECT (#1;", "near \";\": syntax error"),
        ("SELECT #1 #2;", "near \"#2\": syntax error"),
        ("SELECT (#1", "incomplete input"),
    ] {
        assert_eq!(parse_msg(&c, sql), msg, "for {sql}");
    }
    // A `#name` variable that does not start with a digit is an ordinary
    // named parameter (NULL when unbound), as are `$x(1)` and `:a::b`.
    for ok in ["SELECT #abc", "SELECT $x(1)", "SELECT :a::b"] {
        assert_eq!(c.query(ok).unwrap().rows.len(), 1, "for {ok}");
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
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_end()
            .to_string()
    };
    for sql in [
        "SELECT ^",
        "SELECT #",
        "SELECT @",
        "SELECT $",
        "SELECT :",
        "SELECT 'abc",
        "SELECT \"abc",
        "SELECT x'zz'",
        "SELECT x'1",
        "SELECT x'1'",
        "SELECT x'abc'",
        "SELECT 0xGG",
        "SELECT 0x",
        // A too-big hex literal is recognized then rejected with its own
        // message; the positive form matches sqlite verbatim. (A leading
        // unary minus, `-0x1…`, is a separate token to the lexer, so sqlite's
        // sign-prefixed `-0x1…` echo is a documented residual and not listed.)
        "SELECT 0x10000000000000000",
        "SELECT 0x1ffffffffffffffff",
        "SELECT 0xFFFFFFFFFFFFFFFFF",
        "SELECT 1e",
        "SELECT 1e+",
        "SELECT 123abc",
        "SELECT 1.5xyz",
        "SELECT 12e3f",
        "SELECT 1$",
        "SELECT !",
        "SELECT [abc",
        // misplaced `_` digit separators echo sqlite's mid-dequote buffer
        "SELECT 1_",
        "SELECT 1__2",
        "SELECT 1_2_",
        "SELECT 0x1_2_",
        "SELECT 1_.5",
        "SELECT 0x1_0000000000000000",
        // TCL-style variable subscripts
        "SELECT $x( )",
        "SELECT $x(abc;",
        // an unterminated block comment is whitespace, not an error (a bare
        // `/*` — a division-then-star to the library — is not comparable here:
        // graphite's shell strips it as a comment before the engine sees it,
        // so it is asserted at the library level instead)
        "SELECT /* unterminated",
        "SELECT 1 /* x",
        "/* nothing but comment",
        // a leading UTF-8 BOM is skipped; attached to a word it is an
        // identifier byte instead
        "\u{feff}SELECT 1",
        "SELECT\u{feff} 1",
        // `#N` register references are a syntax error in user SQL; `#name` is
        // an ordinary variable
        "SELECT #1",
        "SELECT #1;",
        "SELECT 1+#2;",
        "SELECT #1abc;",
        "SELECT (#1);",
        "SELECT (#1;",
        "SELECT (#1",
        "SELECT #1 #2;",
        "#1",
        "SELECT #abc;",
        "SELECT :a::b;",
        "SELECT $x(1);",
        // `$` in an identifier
        "SELECT a$b;",
        // valid tokens still lex (no false rejection); typeof() keeps the
        // compared output to a type name, avoiding CLI value-rendering quirks
        // (e.g. a control-byte blob) that are unrelated to lexing.
        "SELECT typeof(x'1f')",
        "SELECT typeof(0x1f)",
        "SELECT typeof(1.5e3)",
        "SELECT typeof(1_000)",
        "SELECT typeof(0x1_2)",
        "SELECT 1e5_3",
        "SELECT 5.e2",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
