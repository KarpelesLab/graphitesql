//! The `fullkey`/`path` columns of `json_each`/`json_tree` double-quote an object
//! key unless it is a "simple" label — non-empty, an ASCII letter followed only
//! by ASCII alphanumerics. A leading digit or `_`, a space, a `.`, a non-ASCII
//! character, etc. force `."<json-escaped>"` (e.g. `$."a b"`, `$."_x"`,
//! `$."a\"b"`). graphite concatenated the raw key (`$.a b`), diverging from
//! SQLite for every non-trivial key. Verified against the sqlite3 3.50.4 CLI.

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
fn simple_labels_stay_bare_others_are_quoted() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Bare: starts with an ASCII letter, all ASCII alphanumerics.
    assert_eq!(
        run(g, "SELECT fullkey FROM json_each('{\"abc\":1}')"),
        "$.abc"
    );
    assert_eq!(
        run(g, "SELECT fullkey FROM json_each('{\"a9\":1}')"),
        "$.a9"
    );
    // Quoted: leading digit, underscore, space, dot, dash.
    assert_eq!(
        run(g, "SELECT fullkey FROM json_each('{\"1a\":1}')"),
        "$.\"1a\""
    );
    assert_eq!(
        run(g, "SELECT fullkey FROM json_each('{\"_x\":1}')"),
        "$.\"_x\""
    );
    assert_eq!(
        run(g, "SELECT fullkey FROM json_each('{\"a b\":1}')"),
        "$.\"a b\""
    );
    // Nested keys are each quoted independently in the path.
    assert_eq!(
        run(g, "SELECT fullkey FROM json_tree('{\"a.b\":{\"c d\":1}}')"),
        "$\n$.\"a.b\"\n$.\"a.b\".\"c d\""
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
        "SELECT fullkey,path FROM json_tree('{\"a b\":{\"c.d\":1}}')",
        "SELECT key,fullkey FROM json_each('{\"_x\":1,\"a1\":2,\"1a\":3,\"über\":4}')",
        "SELECT fullkey,path FROM json_tree('{\"a\":{\"b c\":[10,20]}}')",
        "SELECT fullkey,path FROM json_each('{\"ok\":1,\"no-no\":2,\"\":3}')",
        // A key containing a quote / backslash is JSON-escaped inside the quotes.
        "SELECT fullkey FROM json_tree('{\"a\\\"b\":1}')",
        "SELECT fullkey FROM json_tree('{\"a\\\\b\":1}')",
        // Mixed with array indices, which are never quoted.
        "SELECT fullkey,path FROM json_tree('{\"x y\":[{\"z\":9}]}')",
        // Path-rooted walks keep the user path prefix and quote child keys.
        "SELECT fullkey FROM json_each('{\"grp\":{\"a b\":1,\"cd\":2}}','$.grp')",
        // Uppercase stays bare; digits-after-letter stay bare.
        "SELECT fullkey FROM json_each('{\"ABC\":1,\"A1B2\":2}')",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
