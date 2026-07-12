//! Differential testing of the char-semantic scalar functions — `length`,
//! `unicode`, `substr` — applied to *non-UTF-8* text against the real `sqlite3`
//! CLI (the 3.50.4 oracle on PATH).
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
}
