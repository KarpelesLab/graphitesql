//! `lag`/`lead`'s offset argument must be integer-valued under numeric affinity.
//! SQLite accepts `2.0`, `'2'`, `'2x'`, and `1+1` (all whole numbers) but treats
//! a genuinely fractional offset (`1.9`, `2.5`, `1.5+0.5`→no, `v*0+1.5`) — and a
//! `NULL` offset — as out of range, so the row's *default* is returned (`NULL`
//! when no third argument is given). graphite previously truncated the offset to
//! an integer and returned a shifted value. Verified against the sqlite3 3.50.4
//! CLI.

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

const DDL: &str = "CREATE TABLE t(id INT, v INT); \
     INSERT INTO t VALUES(1,10),(2,20),(3,30),(4,40);";

#[test]
fn lag_lead_offset_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for call in [
        // Fractional offsets -> default (NULL here) for every row.
        "lag(v,1.9)",
        "lead(v,2.5)",
        "lag(v,v*0+1.5)",
        "lag(v,NULL)",
        // With a third argument, a fractional offset returns that default.
        "lag(v,1.9,'D')",
        "lead(v,2.5,-1)",
        // Whole-number offsets (int, int-valued real, numeric text, expression).
        "lag(v)",
        "lag(v,1)",
        "lead(v,2)",
        "lag(v,1.0)",
        "lag(v,2.0)",
        "lag(v,'2')",
        "lag(v,'2x')",
        "lag(v,1.5+0.5)",
        "lag(v,2,99)",
    ] {
        let sql = format!("{DDL} SELECT id, {call} OVER (ORDER BY id) FROM t;");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {call}");
    }
}
