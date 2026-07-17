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

#[test]
fn function_list_is_populated_sorted_and_sqlite_shaped() {
    let c = Connection::open_memory().unwrap();
    let r = c.query("PRAGMA function_list").unwrap();
    // Same six columns as sqlite's `PRAGMA function_list`.
    assert_eq!(
        r.columns,
        vec!["name", "builtin", "type", "enc", "narg", "flags"]
    );
    assert!(
        r.rows.len() > 100,
        "expected many functions, got {}",
        r.rows.len()
    );

    // Every row: builtin is 1, enc is utf8, type is one of s/a/w.
    for row in &r.rows {
        assert_eq!(row[1], Value::Integer(1), "builtin must be 1");
        assert_eq!(row[3], Value::Text("utf8".into()), "enc must be utf8");
        match &row[2] {
            Value::Text(t) => assert!(
                t == "s" || t == "a" || t == "w",
                "type must be s/a/w, got {t}"
            ),
            other => panic!("type must be text, got {other:?}"),
        }
    }

    // Sorted by name (sqlite orders `function_list` alphabetically).
    let ns = names(&c, "PRAGMA function_list");
    let mut sorted = ns.clone();
    sorted.sort();
    assert_eq!(ns, sorted, "function_list must be sorted by name");

    // A spread of functions graphite genuinely registers must be present.
    for want in [
        "abs",
        "coalesce",
        "json_extract",
        "printf",
        "substr",
        "sum",
        "count",
        "row_number",
        "sqlite_compileoption_used",
        "sqlite_compileoption_get",
    ] {
        assert!(ns.iter().any(|n| n == want), "function_list missing {want}");
    }
    #[cfg(feature = "fts5")]
    for want in ["bm25", "highlight", "snippet"] {
        assert!(
            ns.iter().any(|n| n == want),
            "function_list missing fts5 {want}"
        );
    }
}

#[test]
fn function_list_reports_kind_and_arity() {
    let c = Connection::open_memory().unwrap();
    let r = c.query("PRAGMA function_list").unwrap();
    let find = |name: &str, kind: &str| -> Option<i64> {
        r.rows.iter().find_map(|row| {
            let n = if let Value::Text(t) = &row[0] {
                t.to_string()
            } else {
                return None;
            };
            let k = if let Value::Text(t) = &row[2] {
                t.to_string()
            } else {
                return None;
            };
            match row[4] {
                Value::Integer(narg) if n == name && k == kind => Some(narg),
                _ => None,
            }
        })
    };
    // Scalar with a fixed arity.
    assert_eq!(find("abs", "s"), Some(1));
    // Variadic scalar reports -1.
    assert_eq!(find("coalesce", "s"), Some(-1));
    // Aggregate kind 'a'.
    assert_eq!(find("sum", "a"), Some(1));
    // Window kind 'w'.
    assert_eq!(find("row_number", "w"), Some(0));
    // min/max appear as both scalar and aggregate.
    assert_eq!(find("min", "s"), Some(-1));
    assert_eq!(find("min", "a"), Some(1));
}

/// The four introspection lists are also exposed as eponymous table-valued
/// functions (`SELECT … FROM pragma_<name>`), exactly as sqlite does — usable in
/// a FROM clause and drivable by `WHERE`, not just as a bare `PRAGMA` statement.
#[test]
fn introspection_lists_usable_as_table_valued_functions() {
    let c = Connection::open_memory().unwrap();
    for tvf in [
        "pragma_function_list",
        "pragma_module_list",
        "pragma_pragma_list",
        "pragma_compile_options",
    ] {
        let sql = alloc_count(tvf);
        let n = match &c.query(&sql).unwrap().rows[0][0] {
            Value::Integer(n) => *n,
            other => panic!("{tvf}: expected integer count, got {other:?}"),
        };
        assert!(n > 0, "{tvf} TVF returned no rows");
    }
    // Drivable by an equality constraint on the exposed column, like sqlite.
    let hit = names(
        &c,
        "SELECT name FROM pragma_function_list WHERE name = 'abs'",
    );
    assert_eq!(hit, vec!["abs".to_string()]);
}

fn alloc_count(tvf: &str) -> String {
    let mut s = String::from("SELECT count(*) FROM ");
    s.push_str(tvf);
    s
}
