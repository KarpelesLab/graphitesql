//! `json_quote(X)` on a BLOB argument: SQLite accepts a blob that decodes as
//! **JSONB** and renders it as the corresponding JSON text (so `jsonb_*` results
//! compose through `json_quote`), and only raises `JSON cannot hold BLOB values`
//! for a blob that is *not* valid JSONB. graphite previously rejected every blob
//! unconditionally — so `json_quote(jsonb('[1,2]'))` errored where SQLite returns
//! `[1,2]`, and the 1-byte JSONB scalars `x'00'`/`x'01'` (JSONB `null`/`true`)
//! errored where SQLite returns `null`/`true`.
//!
//! graphite now routes a blob through its JSONB decoder (the same one behind
//! `jsonb()` / `jsonb_extract`): a valid JSONB blob renders as its JSON text, an
//! invalid one still raises `JSON cannot hold BLOB values`.
//!
//! Verified against the sqlite3 3.50.4 CLI. (The CLI's contextual error prefix —
//! `stepping, ` vs graphite's `error: ` — is stripped, as in the sibling suites;
//! the library message is byte-identical.)

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    let mut lines = Vec::new();
    for line in s.lines() {
        let mut t = line.trim_end();
        if t.trim_start().starts_with('^') {
            continue;
        }
        for prefix in [
            "Error: ",
            "in prepare, ",
            "stepping, ",
            "SQL error: ",
            "error: ",
        ] {
            t = t.strip_prefix(prefix).unwrap_or(t);
        }
        lines.push(t.to_string());
    }
    lines.join("\n")
}

#[test]
fn json_quote_jsonb_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // 1-byte JSONB scalars: x'00' = null, x'01' = true.
        "SELECT json_quote(x'00')",
        "SELECT json_quote(x'01')",
        "SELECT json_quote(zeroblob(1))",
        // jsonb_*() results compose through json_quote (decoded to JSON text).
        "SELECT json_quote(jsonb('[1,2,3]'))",
        "SELECT json_quote(jsonb('{\"a\":1,\"b\":[true,null]}'))",
        "SELECT json_quote(jsonb('null'))",
        "SELECT json_quote(jsonb('true'))",
        "SELECT json_quote(jsonb('false'))",
        "SELECT json_quote(jsonb(123))",
        "SELECT json_quote(jsonb(3.5))",
        "SELECT json_quote(jsonb('\"hi\"'))",
        "SELECT json_quote(jsonb('\"a\\\"b\"'))",
        // Nested: a jsonb array built from a jsonb element.
        "SELECT json_quote(jsonb_array(1, jsonb('[2,3]'), 'x'))",
        // Invalid JSONB blobs are still rejected (message byte-identical).
        "SELECT json_quote(x'')",
        "SELECT json_quote(x'0000')",
        "SELECT json_quote(x'414243')",
        "SELECT json_quote(zeroblob(0))",
        // Non-blob arguments are unchanged by this fix.
        "SELECT json_quote('plain')",
        "SELECT json_quote('a\"b')",
        "SELECT json_quote(1)",
        "SELECT json_quote(1.5)",
        "SELECT json_quote(NULL)",
        "SELECT json_quote(json('[1,2]'))",
        // Over a table column carrying jsonb blobs.
        "CREATE TABLE t(j); INSERT INTO t VALUES(jsonb('[1]')),(jsonb('{\"k\":9}')),(jsonb('null')); \
         SELECT json_quote(j) FROM t ORDER BY rowid",
    ];
    for sql in cases {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
