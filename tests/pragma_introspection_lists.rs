//! `PRAGMA pragma_list` / `module_list` / `compile_options` — introspection over
//! graphite's own registries. Each returns non-empty, sensible rows with sqlite's
//! column shape; the content is graphite's true capability set (not a copy of any
//! particular sqlite build's list), so these are checked structurally, not
//! byte-for-byte against the oracle.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn names(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.to_string(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

#[test]
fn pragma_list_is_populated_sorted_and_self_describing() {
    let c = Connection::open_memory().unwrap();
    let r = c.query("PRAGMA pragma_list").unwrap();
    assert_eq!(r.columns, vec!["name"]);
    let ns = names(&c, "PRAGMA pragma_list");
    assert!(ns.len() > 40, "expected many pragmas, got {}", ns.len());
    // Alphabetical, as sqlite's aPragmaName[] reports.
    let mut sorted = ns.clone();
    sorted.sort();
    assert_eq!(ns, sorted, "pragma_list must be alphabetical");
    // It lists itself and the pragmas graphite genuinely implements.
    for want in [
        "pragma_list",
        "module_list",
        "compile_options",
        "table_info",
        "journal_mode",
        "user_version",
        "wal_checkpoint",
    ] {
        assert!(ns.iter().any(|n| n == want), "pragma_list missing {want}");
    }
}

#[test]
fn module_list_reports_builtin_modules() {
    let c = Connection::open_memory().unwrap();
    let r = c.query("PRAGMA module_list").unwrap();
    assert_eq!(r.columns, vec!["name"]);
    let ns = names(&c, "PRAGMA module_list");
    for want in ["rtree", "rtree_i32", "geopoly", "generate_series", "dbstat"] {
        assert!(ns.iter().any(|n| n == want), "module_list missing {want}");
    }
    #[cfg(feature = "fts5")]
    assert!(ns.iter().any(|n| n == "fts5"), "module_list missing fts5");
}

#[test]
fn compile_options_reports_enabled_features() {
    let c = Connection::open_memory().unwrap();
    let r = c.query("PRAGMA compile_options").unwrap();
    assert_eq!(r.columns, vec!["compile_options"]);
    let ns = names(&c, "PRAGMA compile_options");
    assert!(!ns.is_empty());
    for want in ["ENABLE_RTREE", "ENABLE_GEOPOLY", "ENABLE_MATH_FUNCTIONS"] {
        assert!(
            ns.iter().any(|n| n == want),
            "compile_options missing {want}"
        );
    }
    #[cfg(feature = "fts5")]
    assert!(
        ns.iter().any(|n| n == "ENABLE_FTS5"),
        "compile_options should report ENABLE_FTS5 when built with fts5"
    );
}
