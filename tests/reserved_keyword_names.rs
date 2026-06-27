//! SQLite's grammar splits keywords into a reserved set and a `%fallback` set:
//! a fallback keyword (e.g. `key`, `offset`, `view`) may be used as a bare
//! name, but a reserved one (e.g. `select`, `from`, `index`) may not — it is a
//! `near "KW": syntax error` in *any* name position (column, table, index,
//! trigger, view, alias), regardless of context. A quoted (`"select"`,
//! `[select]`, `` `select` ``) form is always a valid name.
//!
//! graphite used to accept every bare keyword as an identifier. It now mirrors
//! SQLite's reserved set. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// SQLite's reserved keywords — none may be a bare name.
const RESERVED: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "as",
    "autoincrement",
    "between",
    "case",
    "check",
    "collate",
    "commit",
    "constraint",
    "create",
    "default",
    "deferrable",
    "delete",
    "distinct",
    "drop",
    "else",
    "escape",
    "except",
    "exists",
    "foreign",
    "from",
    "group",
    "having",
    "in",
    "index",
    "insert",
    "intersect",
    "into",
    "is",
    "isnull",
    "join",
    "limit",
    "not",
    "nothing",
    "notnull",
    "null",
    "on",
    "or",
    "order",
    "primary",
    "references",
    "returning",
    "select",
    "set",
    "table",
    "then",
    "to",
    "transaction",
    "union",
    "unique",
    "update",
    "using",
    "values",
    "when",
    "where",
];

/// A sample of `%fallback` keywords that *are* usable as bare names.
const FALLBACK: &[&str] = &[
    "abort",
    "action",
    "after",
    "begin",
    "by",
    "cast",
    "conflict",
    "cross",
    "current_date",
    "end",
    "filter",
    "first",
    "glob",
    "if",
    "key",
    "left",
    "match",
    "natural",
    "offset",
    "outer",
    "over",
    "plan",
    "pragma",
    "query",
    "right",
    "rollback",
    "row",
    "temp",
    "trigger",
    "view",
    "virtual",
    "without",
];

#[test]
fn reserved_words_rejected_as_column_names() {
    for kw in RESERVED {
        let mut c = Connection::open_memory().unwrap();
        let err = c
            .execute(&format!("CREATE TABLE t({kw})"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!("SQL error: near \"{kw}\": syntax error"),
            "column name {kw:?}"
        );
    }
}

#[test]
fn reserved_words_rejected_as_table_and_index_names() {
    for kw in RESERVED {
        let mut c = Connection::open_memory().unwrap();
        let err = c
            .execute(&format!("CREATE TABLE {kw}(a)"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!("SQL error: near \"{kw}\": syntax error"),
            "table {kw:?}"
        );

        let mut c = Connection::open_memory().unwrap();
        c.execute("CREATE TABLE t(a)").unwrap();
        let err = c
            .execute(&format!("CREATE INDEX {kw} ON t(a)"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!("SQL error: near \"{kw}\": syntax error"),
            "index {kw:?}"
        );
    }
}

#[test]
fn reserved_words_rejected_as_aliases() {
    for kw in RESERVED {
        let c = Connection::open_memory().unwrap();
        let err = c
            .query(&format!("SELECT 1 AS {kw}"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!("SQL error: near \"{kw}\": syntax error"),
            "alias {kw:?}"
        );
    }
}

#[test]
fn quoted_reserved_words_are_valid_names() {
    for kw in RESERVED {
        // Double-quoted and bracketed forms both name a column fine.
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("CREATE TABLE t(\"{kw}\")"))
            .unwrap_or_else(|e| panic!("double-quoted {kw:?}: {e}"));

        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("CREATE TABLE t([{kw}])"))
            .unwrap_or_else(|e| panic!("bracketed {kw:?}: {e}"));
    }
}

#[test]
fn fallback_words_are_valid_bare_names() {
    for kw in FALLBACK {
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("CREATE TABLE t({kw})"))
            .unwrap_or_else(|e| panic!("fallback column {kw:?}: {e}"));
    }
}

#[test]
fn matches_sqlite_cli_over_every_keyword() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Classify each probe as accepted vs rejected and compare the two engines.
    let classify = |out: &std::process::Output| -> bool {
        let s = String::from_utf8_lossy(&out.stderr);
        let s2 = String::from_utf8_lossy(&out.stdout);
        let all = format!("{s}{s2}");
        !all.to_lowercase().contains("error")
            && !all.to_lowercase().contains("syntax")
            && !all.to_lowercase().contains("incomplete")
    };
    let all_keywords = RESERVED.iter().chain(FALLBACK.iter());
    for kw in all_keywords {
        for tmpl in ["CREATE TABLE t({})", "CREATE TABLE {}(a)", "SELECT 1 AS {}"] {
            let sql = tmpl.replace("{}", kw);
            let s = Command::new("sqlite3")
                .arg(":memory:")
                .arg(&sql)
                .output()
                .unwrap();
            let gg = Command::new(g).arg(":memory:").arg(&sql).output().unwrap();
            assert_eq!(
                classify(&s),
                classify(&gg),
                "accept/reject mismatch for {sql:?}: sqlite={} graphite={}",
                String::from_utf8_lossy(&s.stderr),
                String::from_utf8_lossy(&gg.stderr),
            );
        }
    }
}
