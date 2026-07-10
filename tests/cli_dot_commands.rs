//! The `graphitesql` shell's dot-commands match the `sqlite3` 3.50.4 CLI
//! byte-for-byte: output modes (`.mode list|csv|column|line|tabs|quote|insert|
//! json`), header toggling (`.headers`, and the implicit header-on from `.mode
//! column`), separators/NULL rendering (`.separator`, `.nullvalue`), result
//! redirection (`.output`/`.once`), CSV import (`.import`), and the `.echo` /
//! `.changes` settings. Each case runs the identical stdin script through both
//! shells and asserts equal stdout (and, where relevant, equal stderr / equal
//! imported data).

#![cfg(feature = "std")]

use std::io::Write;
use std::process::{Command, Stdio};

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run `input` on stdin through `bin DB`, capturing stdout only.
fn run(bin: &str, db: &str, input: &str) -> Vec<u8> {
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap().stdout
}

/// Run `input`, capturing both stdout and stderr (stderr paths file names, so the
/// caller normalizes those out before comparing).
fn run_both(bin: &str, db: &str, input: &str) -> (Vec<u8>, Vec<u8>) {
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (out.stdout, out.stderr)
}

fn graphite() -> &'static str {
    env!("CARGO_BIN_EXE_graphitesql")
}

/// Assert both shells produce identical stdout for the same in-memory script.
fn assert_same(label: &str, script: &str) {
    let s = run("sqlite3", ":memory:", script);
    let g = run(graphite(), ":memory:", script);
    assert_eq!(
        String::from_utf8_lossy(&s),
        String::from_utf8_lossy(&g),
        "stdout mismatch for `{label}`\nscript:\n{script}"
    );
}

#[test]
fn output_modes_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let cases: &[(&str, &str)] = &[
        (
            "list_headers",
            ".headers on\nSELECT 1 AS a, 2 AS b UNION SELECT 3,4;\n",
        ),
        (
            "csv",
            ".mode csv\n.headers on\nSELECT 1 AS a, 'x,y' AS b, NULL AS c;\n",
        ),
        (
            "csv_quoting",
            ".mode csv\nSELECT 'a b', 'plain', '', 'has\"quote', ' lead';\n",
        ),
        (
            "column",
            ".mode column\n.headers on\nSELECT 1 AS a, 'hello' AS bb;\n",
        ),
        (
            "column_widths",
            ".mode column\n.headers on\nSELECT 'x' AS name, 1 AS n UNION SELECT 'longer', 2;\n",
        ),
        (
            "column_pad_last",
            ".mode column\n.headers on\nSELECT 1 AS a, 'longheader' AS bb UNION SELECT 2, 'x';\n",
        ),
        (
            "column_headers_off",
            ".mode column\n.headers off\nSELECT 1 AS a, 2 AS b;\n",
        ),
        (
            "column_implicit_headers",
            ".mode column\nSELECT 1 AS a, 22 AS bb;\n",
        ),
        (
            "line",
            ".mode line\nSELECT 1 AS a, 'x' AS bbbbbbbb UNION SELECT 2, 'y';\n",
        ),
        ("tabs", ".mode tabs\n.headers on\nSELECT 1 AS a, 2 AS b;\n"),
        (
            "quote",
            ".mode quote\n.headers on\nSELECT 1, 'x', NULL, 2.5, x'00ff';\n",
        ),
        (
            "quote_special_reals",
            ".mode quote\nSELECT 5.0, 0.1, 2.0/3.0, 1e400, -1e400;\n",
        ),
        (
            "insert",
            ".mode insert t1\n.headers on\nSELECT 1 AS a, 'x' AS b, NULL;\n",
        ),
        (
            "insert_keyword_names",
            ".mode insert \"order\"\n.headers on\nSELECT 1 AS a, 'x' AS b, NULL;\n",
        ),
        (
            "insert_noheader",
            ".mode insert t1\nSELECT 1 AS a, 'x' AS b, 2.5, x'ab';\n",
        ),
        (
            "json",
            ".mode json\nSELECT 1 AS a, 'x' AS b, NULL AS c, 2.5 AS d;\n",
        ),
        (
            "json_blob_escapes",
            ".mode json\nSELECT x'00ff' AS b, 'a\"b'||char(9) AS s;\n",
        ),
        (
            "json_special_reals",
            ".mode json\nSELECT 9.0e999 AS a, -9e999 AS b, 2.5 AS c;\n",
        ),
        (
            "mode_prefix",
            ".mode col\n.headers on\nSELECT 1 AS a, 2 AS b;\n",
        ),
    ];
    for (label, script) in cases {
        assert_same(label, script);
    }
}

#[test]
fn separator_and_nullvalue_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    assert_same("separator_col", ".separator ,\nSELECT 1,2,3;\n");
    assert_same(
        "separator_col_row",
        ".separator ; :\nSELECT 1,2;\nSELECT 3,4;\n",
    );
    assert_same("nullvalue", ".nullvalue NULL\nSELECT 1, NULL, 3;\n");
    assert_same(
        "nullvalue_csv",
        ".mode csv\n.nullvalue NIL\nSELECT NULL, 1;\n",
    );
    assert_same(
        "nullvalue_column",
        ".mode column\n.headers on\n.nullvalue (null)\nSELECT 1 AS a, NULL AS b;\n",
    );
    assert_same(
        "separator_csv_field",
        ".mode csv\n.separator ;\nSELECT 'a;b', 'c';\n",
    );
}

#[test]
fn echo_and_changes_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    assert_same("echo", ".echo on\nSELECT 1;\nSELECT 2;\n");
    assert_same(
        "echo_multi",
        ".echo on\nCREATE TABLE t(a);\nSELECT 1; SELECT 2;\n",
    );
    assert_same(
        "changes",
        ".changes on\nCREATE TABLE t(a);\nINSERT INTO t VALUES(1),(2),(3);\nUPDATE t SET a=a+1;\nSELECT * FROM t;\n",
    );
    assert_same(
        "changes_persist_over_ddl",
        ".changes on\nCREATE TABLE t(a);\nINSERT INTO t VALUES(1),(2);\nCREATE TABLE u(b);\n",
    );
    assert_same(
        "changes_delete",
        ".changes on\nCREATE TABLE t(a);\nINSERT INTO t VALUES(1),(2);\nDELETE FROM t;\n",
    );
}

#[test]
fn output_and_once_redirect_like_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let dir = std::env::temp_dir();
    let uniq = std::process::id();

    // `.output FILE` … `.output` (revert): the redirected block lands in the file,
    // and output after `.output` returns to stdout. Compare both the file
    // contents and the stdout for the two shells.
    for (tag, revert) in [("revert", ".output\nSELECT 99;\n"), ("once", "")] {
        let sf = dir.join(format!("gsql_out_s_{uniq}_{tag}"));
        let gf = dir.join(format!("gsql_out_g_{uniq}_{tag}"));
        let (sf, gf) = (sf.to_str().unwrap(), gf.to_str().unwrap());
        let _ = std::fs::remove_file(sf);
        let _ = std::fs::remove_file(gf);
        let cmd = if tag == "once" { ".once" } else { ".output" };
        let s_script = format!("{cmd} {sf}\nSELECT 1;\nSELECT 2;\n{revert}");
        let g_script = format!("{cmd} {gf}\nSELECT 1;\nSELECT 2;\n{revert}");
        let s_out = run("sqlite3", ":memory:", &s_script);
        let g_out = run(graphite(), ":memory:", &g_script);
        assert_eq!(
            String::from_utf8_lossy(&s_out),
            String::from_utf8_lossy(&g_out),
            "stdout mismatch for .{cmd} ({tag})"
        );
        let s_file = std::fs::read(sf).unwrap_or_default();
        let g_file = std::fs::read(gf).unwrap_or_default();
        assert_eq!(
            String::from_utf8_lossy(&s_file),
            String::from_utf8_lossy(&g_file),
            "file contents mismatch for .{cmd} ({tag})"
        );
        let _ = std::fs::remove_file(sf);
        let _ = std::fs::remove_file(gf);
    }
}

#[test]
fn import_csv_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let dir = std::env::temp_dir();
    let uniq = std::process::id();

    // (label, csv-bytes, script-template with {F} substituted for the CSV path)
    let cases: &[(&str, &str, &str)] = &[
        (
            "create_from_header",
            "a,b\n1,\"x,y\"\n2,hello\n",
            ".mode csv\n.import {F} t\n.headers on\nSELECT * FROM t ORDER BY a;\n",
        ),
        (
            "into_existing_no_skip",
            "a,b\n1,\"x,y\"\n2,hello\n",
            "CREATE TABLE t(a,b);\n.mode csv\n.import {F} t\nSELECT rowid,* FROM t;\n",
        ),
        (
            "column_mismatch",
            "1\n2,3,4\n",
            "CREATE TABLE t(a,b);\n.mode csv\n.import {F} t\nSELECT * FROM t;\n",
        ),
        (
            "default_separator",
            "1|2\n3|4\n",
            "CREATE TABLE t(a,b);\n.import {F} t\nSELECT * FROM t;\n",
        ),
        (
            "csv_flag",
            "a,b\n1,x\n",
            "CREATE TABLE t(a,b);\n.import --csv {F} t\nSELECT * FROM t;\n",
        ),
        (
            "quoted_fields",
            "x\n\"quo\"\"te\"\n\"comma,here\"\n",
            "CREATE TABLE t(x);\n.mode csv\n.import {F} t\nSELECT quote(x) FROM t;\n",
        ),
    ];

    for (label, csv, tmpl) in cases {
        let csvfile = dir.join(format!("gsql_imp_{uniq}_{label}.csv"));
        let csvfile = csvfile.to_str().unwrap();
        std::fs::write(csvfile, csv).unwrap();
        let script = tmpl.replace("{F}", csvfile);

        // Fresh DB files per shell so imports don't collide.
        let sdb = dir.join(format!("gsql_imp_s_{uniq}_{label}.db"));
        let gdb = dir.join(format!("gsql_imp_g_{uniq}_{label}.db"));
        let (sdb, gdb) = (sdb.to_str().unwrap(), gdb.to_str().unwrap());
        let _ = std::fs::remove_file(sdb);
        let _ = std::fs::remove_file(gdb);

        let (s_out, s_err) = run_both("sqlite3", sdb, &script);
        let (g_out, g_err) = run_both(graphite(), gdb, &script);
        assert_eq!(
            String::from_utf8_lossy(&s_out),
            String::from_utf8_lossy(&g_out),
            "stdout mismatch for .import `{label}`"
        );
        // stderr carries the `FILE:LINE: expected …` warnings; the file path is
        // identical between runs (same CSV), so compare verbatim.
        assert_eq!(
            String::from_utf8_lossy(&s_err),
            String::from_utf8_lossy(&g_err),
            "stderr mismatch for .import `{label}`"
        );
        let _ = std::fs::remove_file(csvfile);
        let _ = std::fs::remove_file(sdb);
        let _ = std::fs::remove_file(gdb);
    }
}

#[test]
fn markdown_box_table_print_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let cases: &[(&str, &str)] = &[
        (
            "markdown_headers",
            ".mode markdown\n.headers on\nSELECT 1 AS a, 'hi' AS bee UNION ALL SELECT 22, 'x';\n",
        ),
        (
            "markdown_no_headers_still_shows",
            ".mode markdown\nSELECT 1 AS a, 2 AS b;\n",
        ),
        (
            "markdown_centered_header",
            ".mode markdown\nSELECT 100 AS x, 'yz' AS long_col;\n",
        ),
        (
            "markdown_empty",
            ".mode markdown\n.headers on\nSELECT 1 AS a WHERE 0;\n",
        ),
        ("markdown_prefix_abbrev", ".mode mark\nSELECT 5 AS n;\n"),
        (
            "box_headers",
            ".mode box\n.headers on\nSELECT 1 AS a, 'hi' AS bee UNION ALL SELECT 22, 'x';\n",
        ),
        ("box_no_headers", ".mode box\nSELECT 1 AS a, 2 AS b;\n"),
        (
            "box_null_and_unicode",
            ".mode box\n.headers on\nSELECT NULL AS a, 'héllo' AS b;\n",
        ),
        ("box_empty", ".mode box\nSELECT 1 WHERE 0;\n"),
        (
            "table_headers",
            ".mode table\n.headers on\nSELECT 1 AS id, 'Alice' AS name, 3.5 AS score UNION ALL SELECT 200, 'Bob', 99.25;\n",
        ),
        ("table_no_headers", ".mode table\nSELECT 1 AS a, 2 AS b;\n"),
        ("print_args", ".print hello world\n.print\n.print done\n"),
        (
            "print_then_query",
            ".mode box\n.print --- results ---\nSELECT 42 AS answer;\n",
        ),
    ];
    for (label, script) in cases {
        assert_same(label, script);
    }
}

#[test]
fn ascii_html_modes_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let cases: &[(&str, &str)] = &[
        (
            "ascii_headers",
            ".mode ascii\n.headers on\nSELECT 1 AS a, 'x' AS b UNION ALL SELECT 2,'y';\n",
        ),
        (
            "ascii_no_headers",
            ".mode ascii\nSELECT 'p' AS a, 'q' AS b;\n",
        ),
        (
            "html_headers_escape",
            ".mode html\n.headers on\nSELECT 1 AS a, '<b>&' AS b;\n",
        ),
        ("html_no_headers", ".mode html\nSELECT 1 AS a, 2 AS b;\n"),
        (
            "html_empty",
            ".mode html\n.headers on\nSELECT 1 AS a WHERE 0;\n",
        ),
        (
            "html_quote_apos_null",
            ".mode html\nSELECT 'a\"b' AS q, '''x' AS ap, NULL AS n;\n",
        ),
        (
            "html_multi_row",
            ".mode html\n.headers on\nSELECT 'A&B' AS x, 'C<D>E' AS y UNION ALL SELECT '1','2';\n",
        ),
    ];
    for (label, script) in cases {
        assert_same(label, script);
    }
}

#[test]
fn tcl_mode_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let cases: &[(&str, &str)] = &[
        (
            "tcl_headers_quote",
            ".mode tcl\n.headers on\nSELECT 1 AS a, 'x y' AS b UNION ALL SELECT 2,'q\"r';\n",
        ),
        (
            "tcl_backslash_null_empty",
            ".mode tcl\nSELECT 'a\\b' AS x, NULL AS n, '' AS e;\n",
        ),
        ("tcl_no_headers", ".mode tcl\nSELECT 1 AS a, 2 AS b;\n"),
        ("tcl_empty", ".mode tcl\n.headers on\nSELECT 1 WHERE 0;\n"),
        (
            "tcl_types_and_nul_truncation",
            ".mode tcl\nSELECT 42, 3.5, x'00ab', 'nul'||char(0)||'after';\n",
        ),
        ("tcl_unicode_passthrough", ".mode tcl\nSELECT 'héllo €';\n"),
        (
            "tcl_control_chars_octal",
            ".mode tcl\nSELECT char(8), char(11), char(127), char(9);\n",
        ),
        ("tcl_blob_octal", ".mode tcl\nSELECT x'deadbeef' AS b;\n"),
    ];
    for (label, script) in cases {
        assert_same(label, script);
    }
}

#[test]
fn show_settings_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let cases: &[(&str, &str)] = &[
        ("defaults", ".show\n"),
        (
            "headers_nullvalue_box",
            ".headers on\n.mode box\n.nullvalue NULL\n.show\n",
        ),
        ("csv", ".mode csv\n.show\n"),
        ("ascii_separators", ".mode ascii\n.show\n"),
        ("tabs_reports_list", ".mode tabs\n.show\n"),
        (
            "markdown_custom_sep",
            ".mode markdown\n.separator ; ROW\n.show\n",
        ),
        ("insert_omits_table", ".mode insert foo\n.show\n"),
        ("column_family_wrap", ".mode column\n.show\n"),
        (
            "line_quote_json_tcl_html",
            ".mode json\n.show\n.mode tcl\n.show\n.mode html\n.show\n",
        ),
    ];
    for (label, script) in cases {
        assert_same(label, script);
    }
}

#[test]
fn control_char_escaping_in_display_modes() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    // The width-aligned display modes caret-escape control chars *before*
    // computing column widths, so the borders line up as sqlite's do.
    let cases: &[(&str, &str)] = &[
        ("box", ".mode box\n.headers on\nSELECT x'024142' AS c;\n"),
        (
            "table",
            ".mode table\n.headers on\nSELECT x'024142' AS c;\n",
        ),
        (
            "markdown",
            ".mode markdown\n.headers on\nSELECT x'0102' AS c;\n",
        ),
        (
            "column",
            ".mode column\n.headers on\nSELECT x'024142' AS c, 'plain' AS d;\n",
        ),
        ("line", ".mode line\nSELECT x'024142' AS c;\n"),
        (
            "box_multirow",
            ".mode box\n.headers on\nSELECT x'1b' AS a, 'wide value' AS b UNION ALL SELECT x'02','x';\n",
        ),
    ];
    for (label, script) in cases {
        assert_same(label, script);
    }
}
