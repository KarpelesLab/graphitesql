//! An `UPDATE`/`DELETE` whose target is an attached-database table may reference
//! a *schema-qualified* table inside its `WHERE`/`SET` subqueries — `main.t`, or
//! even the target's own `aux.u`. graphite swaps the target database into the
//! active `main` slot for the write, but the qualifier→database mapping was not
//! inverted alongside, so `main.t` resolved to the swapped-in aux schema (and
//! `aux.u` to the swapped-out main), erroring `no such table`. Verified against
//! the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run `sql` (prefixed with an `ATTACH` of a fresh temp file as `aux`) through a
/// `:memory:` main database, returning stdout. The attach file is removed first
/// so each engine starts from an empty attached database.
fn run(bin: &str, aux: &std::path::Path, sql: &str) -> String {
    let _ = std::fs::remove_file(aux);
    let full = format!("ATTACH '{}' AS aux;{sql}", aux.display());
    let o = Command::new(bin)
        .arg(":memory:")
        .arg(&full)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(aux);
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn crossdb_write_resolves_qualified_subquery_refs() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let dir = std::env::temp_dir().join(format!("gsql_crossdb_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let aux = dir.join("aux.db");

    let base = "CREATE TABLE main.t(a,b); INSERT INTO main.t VALUES(1,100),(2,200);\
                CREATE TABLE aux.u(a,b); INSERT INTO aux.u VALUES(1,10),(2,20),(3,30);";
    let cases = [
        // WHERE subquery reads main.t (the classic broken case).
        "UPDATE aux.u SET b=b+1 WHERE a IN (SELECT a FROM main.t); SELECT quote(a),quote(b) FROM aux.u ORDER BY a;",
        // SET subquery reads main.t.
        "UPDATE aux.u SET b=(SELECT max(b) FROM main.t) WHERE a<3; SELECT quote(a),quote(b) FROM aux.u ORDER BY a;",
        // DELETE with a main.t subquery.
        "DELETE FROM aux.u WHERE a IN (SELECT a FROM main.t); SELECT quote(a) FROM aux.u ORDER BY a;",
        // The subquery references the target's OWN qualified name.
        "UPDATE aux.u SET b=0 WHERE a IN (SELECT a FROM aux.u WHERE b>15); SELECT quote(a),quote(b) FROM aux.u ORDER BY a;",
        // Reverse direction: main target, aux subquery (already worked — keep it green).
        "UPDATE main.t SET b=b*10 WHERE a IN (SELECT a FROM aux.u); SELECT quote(a),quote(b) FROM main.t ORDER BY a;",
        // No-subquery cross-db write (already worked).
        "UPDATE aux.u SET b=b-1 WHERE a=2; SELECT quote(a),quote(b) FROM aux.u ORDER BY a;",
        // A cross-db read is unaffected.
        "SELECT (SELECT count(*) FROM aux.u), (SELECT count(*) FROM main.t);",
    ];
    for c in cases {
        let sql = format!("{base}{c}");
        assert_eq!(run("sqlite3", &aux, &sql), run(g, &aux, &sql), "for `{c}`");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
