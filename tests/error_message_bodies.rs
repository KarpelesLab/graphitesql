//! Error-message-body parity with sqlite3 3.50.4 for three more cases:
//!   (5) a compound column-count mismatch names the specific operator
//!       (`SELECTs to the left and right of UNION ALL …`), while a genuine
//!       multi-row VALUES with uneven rows keeps `all VALUES must have the same
//!       number of terms`;
//!   (6) a window/ranking function used outside OVER is `misuse of window
//!       function NAME()`, not `no such function`;
//!   (2) a NULL into a WITHOUT ROWID primary-key column names the column
//!       (`NOT NULL constraint failed: t.a`).

#![cfg(feature = "std")]

use graphitesql::Connection;

fn err(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .trim_start_matches("Error: ")
        .to_string()
}

#[test]
fn compound_column_count_mismatch_names_operator() {
    let c = Connection::open_memory().unwrap();
    for (sql, kw) in [
        ("SELECT 1 UNION SELECT 1,2", "UNION"),
        ("SELECT 1 UNION ALL SELECT 1,2", "UNION ALL"),
        ("SELECT 1 INTERSECT SELECT 1,2", "INTERSECT"),
        ("SELECT 1 EXCEPT SELECT 1,2", "EXCEPT"),
    ] {
        assert_eq!(
            err(&c, sql),
            format!("SELECTs to the left and right of {kw} do not have the same number of result columns"),
            "{sql}"
        );
    }
}

#[test]
fn multi_row_values_uneven_keeps_values_message() {
    let c = Connection::open_memory().unwrap();
    // A real VALUES list (auto-aliased column1, column2, …) reports the VALUES
    // wording, distinct from an explicit UNION ALL of FROM-less SELECTs.
    assert_eq!(
        err(&c, "VALUES(1),(2,3)"),
        "all VALUES must have the same number of terms"
    );
    assert_eq!(
        err(&c, "VALUES(1,2),(3)"),
        "all VALUES must have the same number of terms"
    );
    // Explicit (even) SELECT UNION ALL is unaffected.
    assert!(c.query("SELECT 1,2 UNION ALL SELECT 3,4").is_ok());
}

#[test]
fn window_function_without_over_is_misuse() {
    let c = Connection::open_memory().unwrap();
    for (sql, name) in [
        ("SELECT row_number()", "row_number"),
        ("SELECT rank()", "rank"),
        ("SELECT dense_rank()", "dense_rank"),
        ("SELECT percent_rank()", "percent_rank"),
        ("SELECT cume_dist()", "cume_dist"),
        ("SELECT ntile(2)", "ntile"),
        ("SELECT lag(1)", "lag"),
        ("SELECT lead(1)", "lead"),
        ("SELECT first_value(1)", "first_value"),
        ("SELECT last_value(1)", "last_value"),
        ("SELECT nth_value(1,2)", "nth_value"),
    ] {
        assert_eq!(
            err(&c, sql),
            format!("misuse of window function {name}()"),
            "{sql}"
        );
    }
}

#[test]
fn without_rowid_pk_null_names_column() {
    // INSERT goes through execute(); capture its error body.
    let dml_err = |c: &mut Connection, sql: &str| {
        c.execute(sql)
            .unwrap_err()
            .to_string()
            .trim_start_matches("error: ")
            .trim_start_matches("Error: ")
            .to_string()
    };

    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a PRIMARY KEY) WITHOUT ROWID")
        .unwrap();
    assert_eq!(
        dml_err(&mut c, "INSERT INTO t VALUES(NULL)"),
        "NOT NULL constraint failed: t.a"
    );

    let mut c2 = Connection::open_memory().unwrap();
    c2.execute("CREATE TABLE t(a, b, PRIMARY KEY(a, b)) WITHOUT ROWID")
        .unwrap();
    assert_eq!(
        dml_err(&mut c2, "INSERT INTO t VALUES(1, NULL)"),
        "NOT NULL constraint failed: t.b"
    );
}
