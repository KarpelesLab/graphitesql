//! `json_tree(J, path)` reports the root element's `key` and `path` columns using
//! SQLite's `jsonEachPathLength` rule, which is quirky: the trailing path segment
//! becomes the `key` (with `path` = its parent) ONLY when that element is the
//! *first* child of its parent; otherwise the whole suffix after `$.`/`$[` becomes
//! the key and `path` collapses to `$`. So `json_tree(J,'$.b[0]')` reports
//! `key=0, path=$.b`, but `json_tree(J,'$.b[2]')` reports `key='b[2]', path=$`.
//! graphite previously always split at the last segment. Verified byte-for-byte
//! against the sqlite3 3.50.4 CLI (found by a json_tree path fuzzer).

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
fn json_tree_root_key_path_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let doc = r#"{"a":1,"b":[10,20,[30],{"c":4},50],"d":{"e":5},"f":{"g":{"h":9}}}"#;
    let paths = [
        "$", "$.a", "$.b", "$.d", "$.b[0]", "$.b[1]", "$.b[2]", "$.b[3]", "$.b[4]", "$.d.e",
        "$.f.g", "$.f.g.h",
        "$[0]", // (no such top-level index on an object -> no rows, both empty)
    ];
    let mut sql = String::new();
    for p in paths {
        sql.push_str(&format!(
            "SELECT key,value,type,fullkey,path,parent FROM json_tree('{doc}','{p}') ORDER BY id;"
        ));
    }
    assert_eq!(out("sqlite3", &sql), out(g, &sql));

    // A nested array whose selected element is not the first child, reached
    // through several array levels, plus the same via json_each.
    let doc2 = r#"[[10,20,30],[[1],[2,3]],{"k":[9,8,7]}]"#;
    let paths2 = [
        "$[0][2]",
        "$[1][1]",
        "$[1][1][0]",
        "$[2].k[1]",
        "$[2].k[0]",
        "$[0][0]",
    ];
    let mut sql2 = String::new();
    for p in paths2 {
        for fn_ in ["json_tree", "json_each"] {
            sql2.push_str(&format!(
                "SELECT key,fullkey,path FROM {fn_}('{doc2}','{p}') ORDER BY id;"
            ));
        }
    }
    assert_eq!(out("sqlite3", &sql2), out(g, &sql2));
}
