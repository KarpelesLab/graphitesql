//! The `id` (and `parent`) columns of `json_each`/`json_tree` are **not** a plain
//! 0,1,2 row counter — SQLite reports each element's byte offset within the
//! document's internal JSONB encoding. An object member is numbered by its *key*
//! node, an array element / the document root by its own value node, and a node's
//! `parent` is the id of its containing element. Path-rooted walks
//! (`json_each(x, '$.a')`) keep numbering against the *whole* document. graphite
//! used a sequential counter, so every id past the first diverged. Verified
//! against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> String {
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
        .trim_start_matches("Error: stepping, ")
        .trim_start_matches("Error: ")
        .trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .trim_end()
        .to_string()
}

#[test]
fn json_each_id_is_the_jsonb_byte_offset() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // `[10,20,30]`: array header at 0, then a 3-byte element each → 1,4,7.
    assert_eq!(run(g, "SELECT id FROM json_each('[10,20,30]')"), "1\n4\n7");
    // An object member is numbered by its *key* node: `{\"a\":10,\"b\":20}` → 1,6.
    assert_eq!(
        run(g, "SELECT id FROM json_each('{\"a\":10,\"b\":20}')"),
        "1\n6"
    );
    // A WHERE filter does not renumber surviving rows.
    assert_eq!(
        run(g, "SELECT id FROM json_each('[1,2]') WHERE value>1"),
        "3"
    );
    // A scalar (or the document root) is id 0.
    assert_eq!(run(g, "SELECT id FROM json_each('5')"), "0");
}

#[test]
fn json_tree_id_parent_and_path_rooting() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Nested: root 0; key `a` at 2 (parent 0); its elements at 5,8 (parent 2);
    // key `b` at 11 (parent 0).
    assert_eq!(
        run(
            g,
            "SELECT id||':'||coalesce(parent,'') FROM json_tree('{\"a\":[10,20],\"b\":3}')"
        ),
        "0:\n2:0\n5:2\n8:2\n11:0"
    );
    // A path-rooted walk numbers against the whole document, not the subtree:
    // `$.a` is the array element at byte 4, so its children are 4 and 7.
    assert_eq!(
        run(g, "SELECT id FROM json_each('{\"a\":[10,20]}','$.a')"),
        "4\n7"
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        // json_each over arrays, objects, scalars, and a filtered set.
        "SELECT key,value,type,atom,id,parent,fullkey,path FROM json_each('[10,20,30]')",
        "SELECT key,id,parent,fullkey FROM json_each('{\"a\":10,\"b\":20,\"c\":30}')",
        "SELECT id,parent FROM json_each('5')",
        "SELECT id,parent FROM json_each('\"hi\"')",
        "SELECT id FROM json_each('[1,2,3,4]') WHERE value%2=0",
        // Nested, with escapes and a multi-byte (>=12) payload.
        "SELECT key,value,type,id,parent,fullkey FROM json_tree('{\"a\":[10,20],\"b\":3}')",
        "SELECT key,id,parent,fullkey FROM json_tree('[{\"k\":9},{\"m\":[1,2]}]')",
        "SELECT value,id FROM json_each('[\"a\\\"b\",2,\"aaaaaaaaaaaaaa\"]')",
        "SELECT key,id,parent,fullkey FROM json_tree('{\"a\":[],\"b\":{}}')",
        // Path-rooted: ids stay relative to the whole document.
        "SELECT key,value,id,parent FROM json_each('{\"a\":[10,20]}','$.a')",
        "SELECT key,id,parent,fullkey FROM json_tree('{\"x\":99,\"a\":{\"b\":7}}','$.a')",
        "SELECT key,value,id,parent FROM json_each('{\"a\":[1,2]}','$.a')",
        // Floats and a deeper mixed structure.
        "SELECT value,id,parent FROM json_tree('[1.5,{\"q\":[true,null,\"x\"]}]')",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
