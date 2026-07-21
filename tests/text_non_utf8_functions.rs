//! Differential testing of the char-semantic scalar functions — `length`,
//! `octet_length`, `unicode`, `substr`, `quote`, `char` — applied to *non-UTF-8*
//! text against the real `sqlite3` CLI (the 3.50.4 oracle on PATH).
//!
//! Since `Value::Text` became byte-backed, text whose bytes are not valid UTF-8
//! (`x'ff' || x'fe'`, `CAST(<blob> AS TEXT)`) keeps its `text` storage class, so
//! these functions must count / index / slice its *characters* exactly as SQLite
//! does (a lenient UTF-8 stepping — `lengthFunc`, `sqlite3Utf8Read`, `SKIP_UTF8`)
//! rather than collapsing to 0 / NULL / "" through a lossy decode.
//!
//! Byte-valued results (`substr`) are captured with `hex()` so the comparison is
//! exact; `length` / `unicode` are integers.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3(sql: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn render(result: &graphitesql::QueryResult) -> String {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Text(s) => String::from(s.as_str()),
                    Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                    Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_matches(g: &mut Connection, queries: &[&str]) {
    for q in queries {
        let want = sqlite3(&format!("{q};"));
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "function query diverged: {q}");
    }
}

#[test]
fn char_semantic_functions_on_non_utf8_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // This test asserts SQLite's *byte-wise* handling of non-UTF-8 text (the
    // ASCII/no-ICU build, which graphite matches and which CI pins). A Unicode/ICU
    // sqlite build instead lossily decodes invalid bytes to U+FFFD before case
    // folding / pattern matching (`upper(x'ff')` -> `EFBFBD`, and `LIKE` etc. then
    // diverge), so the differential comparison is meaningless there. Feature-probe
    // the oracle and skip on a Unicode build rather than differential-test a
    // compile-time option (see the `ci-vs-local-sqlite-icu` lesson).
    if sqlite3("SELECT hex(upper(x'ff'))") != "FF" {
        eprintln!(
            "oracle is a Unicode sqlite build (mangles invalid UTF-8); skipping byte-wise text test"
        );
        return;
    }
    let mut g = Connection::open_memory().unwrap();

    // ---- length(): counts non-continuation bytes up to the first NUL. `||`
    // yields byte-backed text, so these exercise the non-UTF-8 path.
    assert_matches(
        &mut g,
        &[
            "SELECT length(x'ff' || x'fe')",          // 2 lone lead bytes
            "SELECT length(x'ff' || x'00')",          // NUL truncates -> 1
            "SELECT length(x'c3' || x'a9')",          // 'é' -> 1 char
            "SELECT length(x'c3' || x'28')",          // invalid pair -> 2
            "SELECT length(x'e4' || x'b8' || x'ad')", // a 3-byte CJK char -> 1
            "SELECT length(CAST(x'ff' AS TEXT))",     // 1
            "SELECT length('héllo'), length('abc')",  // valid utf8 unchanged
            "SELECT length('A' || char(0) || 'B')",   // NUL truncation, = 1
            // octet_length(): full byte length, NOT NUL-truncated.
            "SELECT octet_length(x'ff' || x'fe')",        // 2
            "SELECT octet_length('A' || char(0) || 'B')", // 3, unlike length()
            "SELECT octet_length('héllo')",               // 6 bytes
        ],
    );

    // ---- unicode(): first character's codepoint via SQLite's lenient reader.
    assert_matches(
        &mut g,
        &[
            "SELECT unicode(x'ff' || x'fe')", // lone 0xff lead -> U+FFFD 65533
            "SELECT unicode(x'c3' || x'a9')", // 'é' -> 233
            "SELECT unicode(x'c3' || x'28')", // truncated seq -> U+FFFD
            "SELECT unicode(x'e4' || x'b8' || x'ad')", // CJK -> 20013
            "SELECT unicode(CAST(x'ff' AS TEXT))", // 65533
            "SELECT unicode('A'), unicode('é')", // valid utf8 unchanged
        ],
    );

    // ---- substr(): slices characters, returns byte-exact text (checked as hex).
    assert_matches(
        &mut g,
        &[
            "SELECT hex(substr(x'ff' || x'fe' || x'fd', 2, 1))", // FE
            "SELECT hex(substr(x'c3' || x'a9' || x'21', 1, 1))", // C3A9 ('é')
            "SELECT hex(substr(x'c3' || x'a9' || x'21', 2, 1))", // 21 ('!')
            "SELECT hex(substr(x'ff' || x'fe' || x'fd', -1))",   // FD (last char)
            "SELECT hex(substr(x'ff' || x'fe' || x'fd', 2))",    // FEFD
            "SELECT substr('héllo', 2, 2), substr('abcdef', -2)", // valid unchanged
        ],
    );

    // ---- quote(): renders the SQL literal over the raw bytes (NUL-truncated,
    // single quotes doubled), checked as hex so the byte sequence is exact.
    assert_matches(
        &mut g,
        &[
            "SELECT hex(quote(x'ff' || x'fe'))",      // '<ff><fe>'
            "SELECT hex(quote(x'c3' || x'28'))",      // '<c3>('
            "SELECT hex(quote(CAST(x'ff' AS TEXT)))", // '<ff>'
            "SELECT quote('abc'), quote('a''b')",     // valid utf8 unchanged
            "SELECT quote('A' || char(0) || 'B')",    // NUL truncates -> 'A'
        ],
    );

    // ---- char(): a surrogate / out-of-range code point encodes to raw WTF-8 /
    // U+FFFD exactly like SQLite's charFunc (checked as hex). Valid code points,
    // including a 4-byte astral char, are unchanged.
    assert_matches(
        &mut g,
        &[
            "SELECT hex(char(0xD800)), hex(char(0xDFFF))", // ED A0 80 / ED BF BF
            "SELECT hex(char(-1)), hex(char(0x110000))",   // out of range -> U+FFFD
            "SELECT char(65, 66, 67)",                     // ABC
            "SELECT hex(char(233)), hex(char(0x4E2D)), hex(char(0x1F600))",
        ],
    );

    // ---- upper()/lower() on a *non-UTF-8* text: fold ASCII letters byte-wise and
    // preserve the invalid bytes (SQLite's byte-wise toupper/tolower on an ASCII
    // build; a Unicode build lossily decodes to U+FFFD first, handled by the
    // whole-test oracle probe above).
    assert_matches(
        &mut g,
        &[
            "SELECT hex(upper(x'ff' || 'a' || x'fe'))",  // FF41FE
            "SELECT hex(lower(x'ff' || 'A' || x'fe'))",  // FF61FE
            "SELECT hex(upper(x'ff' || 'hi' || x'00'))", // FF4849 00 kept
            "SELECT upper('abc'), lower('ABC')",         // ASCII unchanged
        ],
    );

    // ---- replace()/instr()/trim() on non-UTF-8 text (byte-wise / char-index /
    // char-set — none case-fold, so oracle-independent). replace works on raw
    // bytes (a blob pattern matches by its bytes); instr is a 1-based char offset
    // for text but a byte offset when both args are blobs; trim removes whole
    // lenient-UTF-8 units found in the trim set.
    assert_matches(
        &mut g,
        &[
            "SELECT hex(replace(x'ff' || x'fe' || x'ff', x'fe', x'00'))", // FF00FF
            "SELECT hex(replace(x'ff' || 'ab' || x'ff', 'ab', 'X'))",     // FF58FF
            "SELECT replace('hello', 'l', 'L'), replace('aaa', 'a', 'bb')", // valid
            "SELECT instr(x'ff' || x'fe' || x'fd', x'fd')",               // 3 (char offset)
            "SELECT instr(x'ff' || 'x', 'x')",                            // 2 (char offset)
            "SELECT instr(x'0102', x'02')",                               // 2 (both blob: byte)
            "SELECT instr('héllo', 'llo'), instr('abc', '')",             // valid: 3, 1
            "SELECT hex(trim(x'ff' || '  '))",                            // FF (trailing spaces)
            "SELECT hex(ltrim(x'ff' || x'fe', x'ff'))",                   // FE (leading FF)
            "SELECT trim('  hi  '), trim('xxhixx', 'x')",                 // valid: hi, hi
        ],
    );

    // ---- concat()/concat_ws(): concatenate the raw text bytes of each argument
    // (like `||`), so a non-UTF-8 argument or separator is preserved.
    assert_matches(
        &mut g,
        &[
            "SELECT hex(concat(x'ff', 'a', x'fe'))", // FF61FE
            "SELECT hex(concat_ws('-', x'ff' || 'a', x'fe' || 'b'))", // FF612DFE62
            "SELECT concat('a', 'b', 'c'), concat(1, 2, NULL, 3)", // valid: abc, 123
            "SELECT concat_ws('-', 'a', NULL, 'c')", // valid: a-c
        ],
    );

    // ---- printf()/format() %s: the argument's raw text bytes are emitted
    // verbatim (a non-UTF-8 %s no longer collapses through a lossy decode).
    // Cases use single-byte "characters" (each invalid byte is one lenient unit),
    // so byte and character counts coincide and the assertions don't depend on
    // whether SQLite's %s width/precision counts bytes or characters. Numeric
    // conversions (ASCII) are unaffected, so valid output stays byte-identical.
    assert_matches(
        &mut g,
        &[
            "SELECT hex(printf('%s', x'ff' || 'a' || x'fe'))", // FF61FE
            "SELECT hex(printf('%.2s', x'ff' || x'fe' || x'fd'))", // FFFE (2 units)
            "SELECT hex(printf('[%4s]', x'ff' || 'a'))",       // 2020FF61 padded
            "SELECT hex(printf('[%-4s]', x'ff' || 'a'))",      // FF612020 left
            "SELECT printf('%s', 'héllo'), printf('%s|%s', 'a', 'b')", // valid
            "SELECT printf('%d %05d %+d %#x', 42, 7, 3, 255)", // numeric unaffected
            // %q/%Q/%w SQL-escape the raw bytes (double the quote byte).
            "SELECT hex(printf('%q', x'ff' || x'27' || 'z'))", // FF27277A ('' doubled)
            "SELECT hex(printf('%Q', x'ff' || x'fe'))",        // 27FFFE27 wrapped
            "SELECT hex(printf('%w', x'ff' || x'22' || 'z'))", // FF22227A ("" doubled)
            "SELECT printf('%q', 'a''b'), printf('%Q', NULL), printf('%q', NULL)", // valid
            // %c emits the first *character* of the argument's text, repeated. A
            // multi-byte leading character is emitted whole, and a non-UTF-8
            // leading byte contributes the raw bytes of its lenient SKIP_UTF8 unit.
            "SELECT hex(printf('%c', 'élan'))", // C3A9 (whole 'é')
            "SELECT hex(printf('%.3c', 'Z'))",  // 5A5A5A (repeat)
            "SELECT hex(printf('%c', x'ff' || 'a'))", // FF (first unit's raw byte)
            "SELECT hex(printf('%.3c', x'fe'))", // FEFEFE (repeat)
            "SELECT hex(printf('%c', x'c3' || x'28'))", // C3 (unit is one byte)
            "SELECT printf('%c%c%c', 65, 66, 67), printf('%c', 'ab')", // valid: 666, a
        ],
    );

    // ---- LIKE / GLOB over non-UTF-8 text: SQLite's patternCompare reads
    // codepoints with the lenient sqlite3Utf8Read, so both operands are decoded
    // the same lenient way (a lone/invalid lead byte -> U+FFFD). Note two distinct
    // invalid lead bytes both decode to U+FFFD, so they compare *equal* — matching
    // SQLite. Valid-UTF-8 matching is unchanged.
    assert_matches(
        &mut g,
        &[
            "SELECT (x'ff' || 'a') LIKE (x'ff' || 'a')", // 1 self-match
            "SELECT (x'ff' || 'a') LIKE ('_' || 'a')",   // 1  _ matches the byte
            "SELECT (x'ff' || 'abc') LIKE (x'ff' || 'a%')", // 1  % tail
            "SELECT (x'ff' || 'a') LIKE (x'ff' || 'b')", // 0  a != b
            "SELECT (x'ff' || 'x') LIKE (x'fe' || 'x')", // 1  both lead -> U+FFFD
            "SELECT (x'ff' || 'a') GLOB (x'ff' || '?')", // 1  ? matches a
            "SELECT (x'ff' || 'a') GLOB (x'ff' || 'b')", // 0
            "SELECT (x'c3' || x'a9') LIKE '_'",          // 1  'é' is one char
            "SELECT 'apple' LIKE 'a%', 'APPLE' LIKE 'a%', 'abc' GLOB 'a[bc]c'", // valid
        ],
    );
}
