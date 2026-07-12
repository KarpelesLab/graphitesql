//! Track B (EQP): a pure `rowid = a OR rowid = b OR …` equality chain — where the
//! key is the `rowid`/`_rowid_`/`oid` alias *or* the explicit INTEGER PRIMARY KEY
//! column — collapses to a single `SEARCH … USING INTEGER PRIMARY KEY (rowid=?)`,
//! exactly as sqlite plans it (not a `MULTI-INDEX OR`). The collapse is scoped to
//! all-equality chains: an `IN`-list disjunct (`id=3 OR id IN(4,5)`), a non-rowid
//! disjunct (`id=3 OR a=5`), or a mixed chain keeps sqlite's `MULTI-INDEX OR`, so
//! the recognition must decline those. `run_core` re-applies the full WHERE, so the
//! unioned rowid seek is a valid superset. Verified byte-exact against sqlite3 — the
//! plan, and the row *set* (a bare rowid OR seek has no inherent order; ORDER BY is
//! left out so the pre-existing IN/OR sort-elision gap doesn't enter the picture).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn g_eqp(c: &Connection, q: &str) -> String {
    c.query(&format!("EXPLAIN QUERY PLAN {q}"))
        .unwrap()
        .rows
        .iter()
        .filter_map(|r| match r.last() {
            Some(Value::Text(s)) => Some(String::from(s.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Row values as a sorted multiset of `|`-joined records — order-independent, since
/// a bare rowid OR seek has no inherent row order (graphite walks the OR list, sqlite
/// the sorted rowids). The EQP assertions pin the plan; this just pins the contents.
fn g_rows_sorted(c: &Connection, q: &str) -> Vec<String> {
    let mut v = c
        .query(q)
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|val| match val {
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => format!("{f}"),
                    Value::Text(s) => String::from(s.as_str()),
                    Value::Null => String::new(),
                    _ => "?".into(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>();
    v.sort();
    v
}

fn sqlite_out(sql: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn sqlite_rows_sorted(sql: &str) -> Vec<String> {
    let mut v: Vec<String> = sqlite_out(sql)
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect();
    v.sort();
    v
}

fn sqlite_eqp(ddl: &str, q: &str) -> String {
    sqlite_out(&format!("{ddl} EXPLAIN QUERY PLAN {q};"))
        .lines()
        .filter(|l| !l.trim().eq_ignore_ascii_case("QUERY PLAN"))
        .map(|l| l.trim_start_matches(|ch| "|`- ".contains(ch)).to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn conn(ddl: &str) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split_inclusive(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

fn check(full: &str, q: &str) {
    let c = conn(full);
    let g = g_eqp(&c, q);
    assert_eq!(g, sqlite_eqp(full, q), "EQP diverged for {q}");
    assert_eq!(
        g_rows_sorted(&c, q),
        sqlite_rows_sorted(&format!("{full} {q};")),
        "rows diverged for {q}"
    );
}

#[test]
fn rowid_equality_or_chain_collapses_to_single_seek() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // Out-of-order explicit rowids; a secondary index on `a` exists so the planner
    // has a tempting alternative — yet a rowid OR-chain must still bare-seek.
    let full = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b); CREATE INDEX ia ON t(a); \
                INSERT INTO t VALUES (3,5,9),(1,3,7),(2,5,2),(5,1,4),(4,8,6);";

    // Each of these is a pure rowid/IPK equality OR-chain → one INTEGER PRIMARY KEY
    // seek, with no MULTI-INDEX OR node. Before the fix graphite emitted MULTI-INDEX
    // OR for them.
    let collapse: &[&str] = &[
        "SELECT * FROM t WHERE id=3 OR id=5",
        "SELECT * FROM t WHERE id=3 OR id=5 OR id=2",
        // The `rowid` alias spelling of the key.
        "SELECT * FROM t WHERE rowid=3 OR rowid=5",
        // Mixed alias/column spellings still denote the same rowid.
        "SELECT * FROM t WHERE id=3 OR rowid=5",
        // A trailing AND-range only narrows the seeked superset (still one seek).
        "SELECT * FROM t WHERE (id=3 OR id=5) AND a>0",
        // Constant-fold / reversed operand order.
        "SELECT * FROM t WHERE 3=id OR 5=id",
        // A single rowid IN-list is one seek as well (baseline, unchanged).
        "SELECT * FROM t WHERE id IN (3,5)",
    ];
    for &q in collapse {
        let c = conn(full);
        let g = g_eqp(&c, q);
        assert!(
            !g.contains("MULTI-INDEX OR"),
            "rowid OR-chain should collapse to one seek for {q}\n  got: {g}"
        );
        assert_eq!(g, "SEARCH t USING INTEGER PRIMARY KEY (rowid=?)", "for {q}");
        check(full, q);
    }
}

#[test]
fn non_pure_rowid_or_keeps_multi_index_or() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    let full = "CREATE TABLE t(id INTEGER PRIMARY KEY,a,b); CREATE INDEX ia ON t(a); \
                INSERT INTO t VALUES (3,5,9),(1,3,7),(2,5,2),(5,1,4),(4,8,6);";

    // sqlite keeps a `MULTI-INDEX OR` when any disjunct is not a bare rowid equality:
    // an IN-list disjunct, a non-rowid (secondary-index) disjunct, or a mix. The
    // recognition must decline these — verified byte-exact (plan and rows) vs sqlite.
    let keep: &[&str] = &[
        "SELECT * FROM t WHERE id=3 OR a=5",
        "SELECT * FROM t WHERE id=3 OR id IN (4,5)",
        "SELECT * FROM t WHERE id IN (3,5) OR id IN (4,6)",
        "SELECT * FROM t WHERE id=3 OR id=5 OR a=2",
    ];
    for &q in keep {
        let c = conn(full);
        let g = g_eqp(&c, q);
        assert!(
            g.contains("MULTI-INDEX OR"),
            "expected MULTI-INDEX OR for {q}\n  got: {g}"
        );
        check(full, q);
    }
}

#[test]
fn implicit_rowid_or_chain_collapses() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }

    // No INTEGER PRIMARY KEY column: only the `rowid` alias is the key.
    let full = "CREATE TABLE u(a,b); CREATE INDEX ua ON u(a); \
                INSERT INTO u VALUES(5,9),(3,7),(5,2),(1,4);";
    let q = "SELECT * FROM u WHERE rowid=2 OR rowid=4";
    let c = conn(full);
    let g = g_eqp(&c, q);
    assert_eq!(g, "SEARCH u USING INTEGER PRIMARY KEY (rowid=?)");
    check(full, q);

    // An IN-list disjunct still keeps MULTI-INDEX OR here too.
    let q2 = "SELECT * FROM u WHERE rowid=2 OR rowid IN (3,4)";
    let c2 = conn(full);
    assert!(g_eqp(&c2, q2).contains("MULTI-INDEX OR"));
    check(full, q2);
}
