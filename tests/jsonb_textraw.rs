//! `jsonb_set`/`jsonb_insert`/`jsonb_replace` byte-parity with SQLite's JSONB
//! writer: a plain (non-JSON-subtype) SQL text *value* argument is stored as a
//! **TEXTRAW** element (raw bytes, unescaped — `jsonFunctionArgToBlob`), and a
//! *new* object key created from a path label is TEXTRAW for a bare or
//! backslash-free quoted label, TEXT5 (verbatim body) otherwise
//! (`jsonLookupStep`'s `rawKey`). Replacing an existing member keeps the stored
//! label bytes. TEXTRAW elements round-trip through `json(jsonb_set(…))` with
//! canonical escaping. Verified byte-for-byte against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn jsonb_edit_textraw_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Plain text values become TEXTRAW (raw bytes, even with quote/backslash).
        "hex(jsonb_set('{}','$.a','plain'))",
        "hex(jsonb_set('{}','$.a','has \"q\" and back\\slash'))",
        "hex(jsonb_set('{}','$.a','é'))",
        "hex(jsonb_insert('[1,2]','$[#]','x'))",
        // New keys: bare and backslash-free quoted labels are TEXTRAW; a quoted
        // label containing a backslash is TEXT5 with the verbatim body.
        "hex(jsonb_set('{}','$.\"a b\"',1))",
        "hex(jsonb_set('{}','$.\"a\\\"b\"',1))",
        "hex(jsonb_set('{}','$.\"\"','v'))",
        "hex(jsonb_set('{}','$.a.b.c',1))",
        "hex(jsonb_insert('{}','$.x','x'))",
        // Replacing/overwriting an existing member keeps its stored label tag.
        "hex(jsonb_replace('{\"a\":1}','$.a','x'))",
        "hex(jsonb_set('{\"a\":1}','$.a','x'))",
        // JSON-subtype and numeric/NULL/blob values are embedded, not TEXTRAW.
        "hex(jsonb_set('{}','$.a',json('[1,2]')))",
        "hex(jsonb_set('{}','$.a',json_quote('s')))",
        "hex(jsonb_set('{}','$.a',1.5))",
        "hex(jsonb_set('{}','$.a',7))",
        "hex(jsonb_set('{}','$.a',NULL))",
        // TEXTRAW round-trips: blob back in, text render escapes canonically.
        "hex(jsonb_set(jsonb_set('{}','$.a','v'),'$.b','w'))",
        "json(jsonb_set('{}','$.a','has \"q\" and '||char(10)||'nl'))",
        "json_extract(jsonb_set('{}','$.a','q\"z'),'$.a')",
        "(select fullkey from json_each(jsonb_set('{}','$.\"a b\"',1)))",
        // A TEXT5 label with an unknown escape decodes leniently, renders verbatim.
        "hex(jsonb_set('{}','$.\"a\\qb\"',1))",
        "json(jsonb_set('{}','$.\"a\\qb\"',1))",
        // The json_* text forms are unchanged by the TEXTRAW writer.
        "json_set('{}','$.a','he\"llo')",
        "hex(jsonb(json_set('{}','$.a','plain')))",
    ];
    let mut sql = String::new();
    for c in cases {
        sql.push_str(&format!("SELECT {c};"));
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));
}
