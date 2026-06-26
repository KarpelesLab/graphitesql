//! Differential hardening of the json1 function surface against `sqlite3` 3.50.4.
//!
//! Every expected value in this file was observed from the real `sqlite3` CLI
//! (the project's differential oracle) and is hard-coded here. Areas covered:
//! end-relative array paths (`[#]` / `[#-k]`), `bad JSON path` errors,
//! `JSON cannot hold BLOB values` / `labels must be TEXT` errors, multi-path
//! `json_extract`, `json_quote` of special floats, and the right-to-left /
//! whole-document semantics of the mutators.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

/// The single scalar result of `sql`.
fn val(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows.remove(0).remove(0)
}

/// The single scalar result of `sql`, expected to be TEXT.
fn text(c: &Connection, sql: &str) -> String {
    match val(c, sql) {
        Value::Text(s) => s,
        other => panic!("expected text from {sql:?}, got {other:?}"),
    }
}

/// The single scalar result of `sql`, expected to be INTEGER.
fn int(c: &Connection, sql: &str) -> i64 {
    match val(c, sql) {
        Value::Integer(i) => i,
        other => panic!("expected integer from {sql:?}, got {other:?}"),
    }
}

/// Assert that `sql` fails with an error whose message contains `needle`.
fn err_contains(c: &Connection, sql: &str, needle: &str) {
    match c.query(sql) {
        Ok(r) => panic!("expected error containing {needle:?} from {sql:?}, got {r:?}"),
        Err(e) => {
            let msg = alloc_to_string(&e);
            assert!(
                msg.contains(needle),
                "error for {sql:?} was {msg:?}, expected to contain {needle:?}"
            );
        }
    }
}

fn alloc_to_string(e: &graphitesql::Error) -> String {
    format!("{e}")
}

// ---------------------------------------------------------------------------
// End-relative array paths: `[#]` (append / one-past-end) and `[#-k]`.
// ---------------------------------------------------------------------------

#[test]
fn extract_end_relative_index() {
    let c = Connection::open_memory().unwrap();
    // `$[#-1]` is the last element; `$[#-2]` the second-to-last (sqlite3).
    assert_eq!(int(&c, "SELECT json_extract('[1,2,3]','$[#-1]')"), 3);
    assert_eq!(int(&c, "SELECT json_extract('[1,2,3]','$[#-2]')"), 2);
    assert_eq!(
        int(&c, "SELECT json_extract('{\"a\":[1,2,3]}','$.a[#-1]')"),
        3
    );
    // `$[#]` (the append slot) does not resolve for reading -> NULL.
    assert_eq!(
        val(&c, "SELECT json_extract('[1,2,3]','$[#]')"),
        Value::Null
    );
    // Counting past the start misses -> NULL.
    assert_eq!(
        val(&c, "SELECT json_extract('[1,2,3]','$[#-9]')"),
        Value::Null
    );
}

#[test]
fn type_and_length_end_relative() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT json_type('[1,2,3]','$[#-1]')"), "integer");
    assert_eq!(
        int(&c, "SELECT json_array_length('[[1,2],[3,4,5]]','$[#-1]')"),
        3
    );
}

#[test]
fn insert_set_append_with_hash() {
    let c = Connection::open_memory().unwrap();
    // `$[#]` appends, for both json_insert and json_set (sqlite3).
    assert_eq!(text(&c, "SELECT json_insert('[1,2]','$[#]',3)"), "[1,2,3]");
    assert_eq!(text(&c, "SELECT json_set('[1,2]','$[#]',3)"), "[1,2,3]");
    // json_replace only overwrites existing elements, so `$[#]` is a no-op.
    assert_eq!(text(&c, "SELECT json_replace('[1,2]','$[#]',3)"), "[1,2]");
    // Append a JSON value (via json(...)) rather than a quoted string.
    assert_eq!(
        text(&c, "SELECT json_insert('[1,2]','$[#]', json('[3]'))"),
        "[1,2,[3]]"
    );
    // Nested append, and two appends in one call accumulate left-to-right.
    assert_eq!(
        text(&c, "SELECT json_set('{\"a\":[1,2]}','$.a[#]',3)"),
        "{\"a\":[1,2,3]}"
    );
    assert_eq!(
        text(
            &c,
            "SELECT json_insert('{\"a\":[1]}','$.a[#]',2,'$.a[#]',3)"
        ),
        "{\"a\":[1,2,3]}"
    );
}

#[test]
fn set_remove_end_relative() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        text(&c, "SELECT json_set('[1,2,3]','$[#-1]',99)"),
        "[1,2,99]"
    );
    assert_eq!(text(&c, "SELECT json_remove('[1,2,3]','$[#-1]')"), "[1,2]");
    // `$[#-5]` past the start is a no-op for set (does NOT append).
    assert_eq!(text(&c, "SELECT json_set('[1,2]','$[#-5]',9)"), "[1,2]");
}

#[test]
fn set_literal_out_of_range_is_noop() {
    let c = Connection::open_memory().unwrap();
    // A literal index past the end does not grow the array (sqlite3).
    assert_eq!(text(&c, "SELECT json_set('[1,2]','$[5]',9)"), "[1,2]");
    assert_eq!(text(&c, "SELECT json_insert('[1,2]','$[5]',9)"), "[1,2]");
}

// ---------------------------------------------------------------------------
// `bad JSON path` errors (distinct from a well-formed-but-missing path -> NULL).
// ---------------------------------------------------------------------------

#[test]
fn bad_json_path_errors() {
    let c = Connection::open_memory().unwrap();
    // A path not rooted at `$`.
    err_contains(
        &c,
        "SELECT json_extract('{\"a\":1}','a')",
        "bad JSON path: 'a'",
    );
    err_contains(
        &c,
        "SELECT json_set('{\"a\":1}','bad',2)",
        "bad JSON path: 'bad'",
    );
    err_contains(&c, "SELECT json_type('{}','foo')", "bad JSON path: 'foo'");
    err_contains(
        &c,
        "SELECT json_array_length('[]','foo')",
        "bad JSON path: 'foo'",
    );
    err_contains(&c, "SELECT json_remove('{}','foo')", "bad JSON path: 'foo'");
    // A negative *literal* index is a bad path (only `$[#-k]` is end-relative).
    err_contains(
        &c,
        "SELECT json_extract('[1,2,3]','$[-1]')",
        "bad JSON path: '$[-1]'",
    );
    // `#+k` is not valid (`#-k` is).
    err_contains(
        &c,
        "SELECT json_extract('[1,2,3]','$[#+1]')",
        "bad JSON path",
    );
    // A bare `$.` with no key.
    err_contains(&c, "SELECT json_extract('{}','$.')", "bad JSON path");
}

#[test]
fn non_text_path_is_coerced_to_text_then_validated() {
    // SQLite applies its usual text conversion to a path argument before parsing
    // it, so an INTEGER/REAL path that does not spell a `$`-rooted path is a
    // `bad JSON path` (showing the coerced text), not a silently-missing lookup.
    // A BLOB path is likewise read as text — `x'24'` is the byte `$`, the whole
    // document. This holds across every path-taking function.
    let c = Connection::open_memory().unwrap();
    err_contains(
        &c,
        "SELECT json_extract('{\"a\":5}', 1)",
        "bad JSON path: '1'",
    );
    err_contains(
        &c,
        "SELECT json_extract('{\"a\":5}', 1.5)",
        "bad JSON path: '1.5'",
    );
    err_contains(&c, "SELECT json_set('{}', 5, 9)", "bad JSON path: '5'");
    err_contains(
        &c,
        "SELECT json_replace('{\"a\":1}', 2, 9)",
        "bad JSON path: '2'",
    );
    err_contains(&c, "SELECT json_type('{\"a\":1}', 7)", "bad JSON path: '7'");
    err_contains(
        &c,
        "SELECT json_remove('{\"a\":1}', 3)",
        "bad JSON path: '3'",
    );
    // A BLOB that decodes to a valid path text addresses the document.
    assert_eq!(
        text(&c, "SELECT json_extract('{\"a\":5}', x'24')"),
        "{\"a\":5}"
    );
}

#[test]
fn json_extract_null_path_short_circuits_to_null() {
    // In `json_extract`, a NULL among the paths collapses the whole result to
    // NULL — scanning left to right, so a NULL reached before any malformed path
    // wins, but a malformed path that comes first still errors.
    let c = Connection::open_memory().unwrap();
    assert_eq!(val(&c, "SELECT json_extract('[1,2]', NULL)"), Value::Null);
    assert_eq!(
        val(&c, "SELECT json_extract('[1,2]', '$[0]', NULL)"),
        Value::Null
    );
    // NULL first → NULL even though the later `5` is a bad path.
    assert_eq!(
        val(&c, "SELECT json_extract('[1,2]', NULL, 5)"),
        Value::Null
    );
    // Bad path first → it errors before the trailing NULL is seen.
    err_contains(
        &c,
        "SELECT json_extract('[1,2]', 5, NULL)",
        "bad JSON path: '5'",
    );
    // No NULL: two valid paths still build the usual JSON array.
    assert_eq!(
        text(&c, "SELECT json_extract('{\"a\":5}', '$.a', '$.a')"),
        "[5,5]"
    );
}

#[test]
fn missing_path_is_null_not_error() {
    let c = Connection::open_memory().unwrap();
    // A well-formed path that just does not resolve yields NULL.
    assert_eq!(
        val(&c, "SELECT json_extract('{\"a\":1}','$.b')"),
        Value::Null
    );
    assert_eq!(
        val(&c, "SELECT json_extract('[1,2,3]','$[5]')"),
        Value::Null
    );
    assert_eq!(val(&c, "SELECT json_type('{\"a\":1}','$.b')"), Value::Null);
    // A NULL document short-circuits to NULL even with a (would-be-bad) path.
    assert_eq!(val(&c, "SELECT json_type(null,'foo')"), Value::Null);
    assert_eq!(val(&c, "SELECT json_extract(null,'foo')"), Value::Null);
}

// ---------------------------------------------------------------------------
// BLOB rejection: `JSON cannot hold BLOB values` and `labels must be TEXT`.
// ---------------------------------------------------------------------------

#[test]
fn blob_values_rejected() {
    let c = Connection::open_memory().unwrap();
    err_contains(
        &c,
        "SELECT json_quote(x'4142')",
        "JSON cannot hold BLOB values",
    );
    err_contains(
        &c,
        "SELECT json_array(x'4142')",
        "JSON cannot hold BLOB values",
    );
    err_contains(
        &c,
        "SELECT json_object('a',x'4142')",
        "JSON cannot hold BLOB values",
    );
    err_contains(
        &c,
        "SELECT json_insert('{}','$.a',x'4142')",
        "JSON cannot hold BLOB values",
    );
    err_contains(
        &c,
        "SELECT json_set('{}','$.a',x'4142')",
        "JSON cannot hold BLOB values",
    );
}

#[test]
fn json_object_labels_must_be_text() {
    let c = Connection::open_memory().unwrap();
    err_contains(
        &c,
        "SELECT json_object(1,2)",
        "json_object() labels must be TEXT",
    );
    err_contains(
        &c,
        "SELECT json_object(null,2)",
        "json_object() labels must be TEXT",
    );
    err_contains(
        &c,
        "SELECT json_object(1.5,2)",
        "json_object() labels must be TEXT",
    );
    err_contains(
        &c,
        "SELECT json_object(x'4142',1)",
        "json_object() labels must be TEXT",
    );
}

// ---------------------------------------------------------------------------
// `json_quote` of special floats and scalars.
// ---------------------------------------------------------------------------

#[test]
fn json_quote_scalars_and_infinity() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT json_quote('abc')"), "\"abc\"");
    assert_eq!(text(&c, "SELECT json_quote(1)"), "1");
    assert_eq!(text(&c, "SELECT json_quote(1.0)"), "1.0");
    // `json_quote(NULL)` renders the literal text "null" (not SQL NULL).
    assert_eq!(text(&c, "SELECT json_quote(null)"), "null");
    // A non-finite REAL renders as sqlite's quoted-literal form `±9.0e+999`
    // (distinct from a JSON literal `9e999`, which round-trips verbatim).
    assert_eq!(text(&c, "SELECT json_quote(9e999)"), "9.0e+999");
    assert_eq!(text(&c, "SELECT json_quote(-9e999)"), "-9.0e+999");
    // A JSON-literal infinity still round-trips as `9e999`.
    assert_eq!(text(&c, "SELECT json('9e999')"), "9e999");
}

// ---------------------------------------------------------------------------
// Multi-path json_extract, whole-document `$`, scalars.
// ---------------------------------------------------------------------------

#[test]
fn json_extract_multipath_and_whole_doc() {
    let c = Connection::open_memory().unwrap();
    // >1 path -> a JSON array of the extracted values.
    assert_eq!(
        text(&c, "SELECT json_extract('{\"a\":1,\"b\":2}','$.a','$.b')"),
        "[1,2]"
    );
    assert_eq!(
        text(&c, "SELECT json_extract('[1,2,3]','$[0]','$[2]')"),
        "[1,3]"
    );
    // `$` returns the whole document (object/array as minified JSON).
    assert_eq!(text(&c, "SELECT json_extract('[1,2,3]','$')"), "[1,2,3]");
    assert_eq!(
        text(&c, "SELECT json_extract('{\"a\":1}','$')"),
        "{\"a\":1}"
    );
    // `$` on a scalar returns the SQL value.
    assert_eq!(int(&c, "SELECT json_extract('5','$')"), 5);
    assert_eq!(text(&c, "SELECT json_extract('\"hi\"','$')"), "hi");
}

// ---------------------------------------------------------------------------
// Mutator semantics: insert/replace/set, right-to-left remove, whole-doc.
// ---------------------------------------------------------------------------

#[test]
fn set_insert_replace_semantics() {
    let c = Connection::open_memory().unwrap();
    // json_insert: only creates absent members.
    assert_eq!(
        text(&c, "SELECT json_insert('{\"a\":1}','$.a',2,'$.b',3)"),
        "{\"a\":1,\"b\":3}"
    );
    // json_replace: only overwrites present members.
    assert_eq!(
        text(&c, "SELECT json_replace('{\"a\":1}','$.a',2,'$.b',3)"),
        "{\"a\":2}"
    );
    // json_set: upsert.
    assert_eq!(
        text(&c, "SELECT json_set('{\"a\":1}','$.a',2,'$.b',3)"),
        "{\"a\":2,\"b\":3}"
    );
}

#[test]
fn remove_multiple_paths_and_whole_doc() {
    let c = Connection::open_memory().unwrap();
    // Paths apply in order; sqlite re-evaluates indices after each removal,
    // so removing `$[0]` then `$[1]` drops the 1st and (original) 3rd elements.
    assert_eq!(
        text(&c, "SELECT json_remove('[1,2,3,4]','$[0]','$[1]')"),
        "[2,4]"
    );
    // Removing the whole document (`$`) yields SQL NULL.
    assert_eq!(val(&c, "SELECT json_remove('{\"a\":1}','$')"), Value::Null);
    assert_eq!(
        val(&c, "SELECT json_remove('{\"a\":1,\"b\":2}','$.a','$')"),
        Value::Null
    );
}

// ---------------------------------------------------------------------------
// Constructor nesting / JSON embedding, json_patch (RFC 7396).
// ---------------------------------------------------------------------------

#[test]
fn constructors_embed_json_vs_string() {
    let c = Connection::open_memory().unwrap();
    // A bare string is quoted; json(...) embeds parsed JSON.
    assert_eq!(text(&c, "SELECT json_array('[1,2]')"), "[\"[1,2]\"]");
    assert_eq!(text(&c, "SELECT json_array(json('[1,2]'))"), "[[1,2]]");
    assert_eq!(
        text(&c, "SELECT json_object('a',json('[1,2]'))"),
        "{\"a\":[1,2]}"
    );
    assert_eq!(
        text(&c, "SELECT json_object('a','[1,2]')"),
        "{\"a\":\"[1,2]\"}"
    );
    // Nested constructors.
    assert_eq!(
        text(&c, "SELECT json_array(json_array(1,2),json_array(3))"),
        "[[1,2],[3]]"
    );
    // Duplicate object keys are kept as-is (sqlite3 does not dedup here).
    assert_eq!(
        text(&c, "SELECT json_object('a',1,'a',2)"),
        "{\"a\":1,\"a\":2}"
    );
}

#[test]
fn json_patch_merge_semantics() {
    let c = Connection::open_memory().unwrap();
    // A null patch member deletes the key; new keys are added.
    assert_eq!(
        text(
            &c,
            "SELECT json_patch('{\"a\":1,\"b\":2}','{\"b\":null,\"c\":3}')"
        ),
        "{\"a\":1,\"c\":3}"
    );
    // Nested objects merge recursively.
    assert_eq!(
        text(
            &c,
            "SELECT json_patch('{\"a\":{\"x\":1}}','{\"a\":{\"y\":2}}')"
        ),
        "{\"a\":{\"x\":1,\"y\":2}}"
    );
    // A non-object patch value replaces the target outright.
    assert_eq!(
        text(&c, "SELECT json_patch('{\"a\":1}','{\"a\":{\"b\":2}}')"),
        "{\"a\":{\"b\":2}}"
    );
    assert_eq!(
        text(&c, "SELECT json_patch('[1,2]','{\"a\":1}')"),
        "{\"a\":1}"
    );
}

// ---------------------------------------------------------------------------
// The `->` / `->>` operators, including negative integer indices (`$[#-k]`).
// ---------------------------------------------------------------------------

#[test]
fn arrow_operators_negative_index() {
    let c = Connection::open_memory().unwrap();
    // `-> -k` addresses the k-th element from the end (== `$[#-k]`).
    assert_eq!(text(&c, "SELECT '[1,2,3]' -> -1"), "3");
    assert_eq!(int(&c, "SELECT '[1,2,3]' ->> -1"), 3);
    assert_eq!(int(&c, "SELECT '[1,2,3,4,5]' ->> -2"), 4);
    // Non-negative index and object label still work.
    assert_eq!(int(&c, "SELECT '[1,2,3]' ->> 0"), 1);
    assert_eq!(int(&c, "SELECT '{\"a\":5}' ->> 'a'"), 5);
}
