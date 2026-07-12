//! Phase 9: multi-table INNER / LEFT joins.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    c.execute("CREATE TABLE orders(id INTEGER PRIMARY KEY, user_id INT, amount INT)")
        .unwrap();
    c.execute("INSERT INTO users(id, name) VALUES (1,'ada'),(2,'grace'),(3,'edsger')")
        .unwrap();
    // ada has 2 orders, grace 1, edsger none.
    c.execute("INSERT INTO orders(user_id, amount) VALUES (1,10),(1,20),(2,30)")
        .unwrap();
    c
}

#[test]
fn inner_join_on() {
    let c = setup();
    let r = c
        .query(
            "SELECT users.name, orders.amount FROM users JOIN orders ON users.id = orders.user_id \
             ORDER BY orders.amount",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 3); // edsger excluded (no orders)
    assert_eq!(r.rows[0][0], Value::Text("ada".into()));
    assert_eq!(r.rows[0][1], Value::Integer(10));
    assert_eq!(r.rows[2][0], Value::Text("grace".into()));
    assert_eq!(r.rows[2][1], Value::Integer(30));
}

#[test]
fn inner_join_with_aggregate() {
    let c = setup();
    let r = c
        .query(
            "SELECT users.name, sum(orders.amount) AS total \
             FROM users JOIN orders ON users.id = orders.user_id \
             GROUP BY users.name ORDER BY total DESC",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], Value::Text("ada".into()));
    assert_eq!(r.rows[0][1], Value::Integer(30)); // 10 + 20
    assert_eq!(r.rows[1][0], Value::Text("grace".into()));
    assert_eq!(r.rows[1][1], Value::Integer(30));
}

#[test]
fn left_join_keeps_unmatched() {
    let c = setup();
    let r = c
        .query(
            "SELECT users.name, orders.amount FROM users LEFT JOIN orders \
             ON users.id = orders.user_id ORDER BY users.name, orders.amount",
        )
        .unwrap();
    // ada(10), ada(20), edsger(NULL), grace(30) -> 4 rows
    assert_eq!(r.rows.len(), 4);
    // edsger has a NULL amount from the left join.
    let edsger = r
        .rows
        .iter()
        .find(|row| row[0] == Value::Text("edsger".into()))
        .unwrap();
    assert_eq!(edsger[1], Value::Null);
}

#[test]
fn comma_join_is_cross_product_filtered_by_where() {
    let c = setup();
    let r = c
        .query(
            "SELECT users.name, orders.amount FROM users, orders \
             WHERE users.id = orders.user_id AND orders.amount >= 20 ORDER BY orders.amount",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 2); // amounts 20 and 30
    assert_eq!(r.rows[0][1], Value::Integer(20));
    assert_eq!(r.rows[1][1], Value::Integer(30));
}

#[test]
fn aliased_join() {
    let c = setup();
    let r = c
        .query(
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id \
             WHERE o.amount = 30",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Text("grace".into()));
}

/// Hash-join correctness across the tricky cases the equi-join hash must handle:
/// integer/real numeric equality, affinity-driven cross-type (`int = text`),
/// NOCASE collation (must fall back to the nested loop and still be correct),
/// duplicate keys, NULLs, and all outer-join kinds. Compared byte-for-byte with
/// real sqlite3.
#[test]
fn hash_join_matches_sqlite3() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-hashjoin-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "\
        CREATE TABLE l(id INTEGER PRIMARY KEY, k, t TEXT, c TEXT COLLATE NOCASE);\
        CREATE TABLE r(id INTEGER PRIMARY KEY, k, t TEXT, c TEXT COLLATE NOCASE);\
        INSERT INTO l(k,t,c) VALUES (1,'1','A'),(2,'2','b'),(2,'2','B'),(3,'x','c'),(5,'5','d'),(NULL,'n','e'),(5.0,'5.0','f');\
        INSERT INTO r(k,t,c) VALUES (1,'1','a'),(2,'2','B'),(5,'5','D'),(5,'x','d'),(NULL,'m','g'),(2.0,'2','h');";
    Command::new("sqlite3")
        .arg(&path)
        .arg(setup)
        .output()
        .unwrap();

    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let queries = [
        // Numeric equi-join (int and real keys collide: 5 vs 5.0, 2 vs 2.0).
        "SELECT l.id, r.id FROM l JOIN r ON l.k = r.k ORDER BY l.id, r.id",
        // Affinity cross-type: numeric column = text column.
        "SELECT l.id, r.id FROM l JOIN r ON l.k = r.t ORDER BY l.id, r.id",
        "SELECT l.id, r.id FROM l JOIN r ON l.t = r.k ORDER BY l.id, r.id",
        // Pure text equi-join.
        "SELECT l.id, r.id FROM l JOIN r ON l.t = r.t ORDER BY l.id, r.id",
        // NOCASE collation join (must fall back; 'A'='a' etc.).
        "SELECT l.id, r.id FROM l JOIN r ON l.c = r.c ORDER BY l.id, r.id",
        // Extra non-equi condition alongside the equi key.
        "SELECT l.id, r.id FROM l JOIN r ON l.k = r.k AND l.id < r.id ORDER BY l.id, r.id",
        // Outer joins exercise the matched-tracking under the hash.
        "SELECT l.id, r.id FROM l LEFT JOIN r ON l.k = r.k ORDER BY l.id, r.id",
        "SELECT l.id, r.id FROM l LEFT JOIN r ON l.k = r.k WHERE r.id IS NULL ORDER BY l.id",
        "SELECT count(*) FROM l JOIN r ON l.k = r.k",
        // Self-join.
        "SELECT a.id, b.id FROM l a JOIN l b ON a.k = b.k AND a.id < b.id ORDER BY a.id, b.id",
    ];
    let render = |v: &Value| match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => String::from(s.as_str()),
        Value::Real(r) => format!("{r}"),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    };
    for q in queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(&path)
                .arg(format!("{q};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = g
            .query(q)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(got, want, "hash-join diverged on {q}");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn indexed_by_nonexistent_index_errors() {
    // `INDEXED BY <name>` requires the index to exist on the table — sqlite errors
    // "no such index" otherwise; graphite used to silently ignore the bogus hint.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT, b INT)").unwrap();
    c.execute("CREATE INDEX i ON t(a)").unwrap();
    c.execute("INSERT INTO t VALUES (1,10),(2,20)").unwrap();
    // A bogus index name is rejected.
    let err = c
        .query("SELECT a FROM t INDEXED BY nope WHERE a=1")
        .unwrap_err();
    assert!(format!("{err}").contains("no such index"), "{err}");
    // A real index (and NOT INDEXED) still works.
    assert!(c.query("SELECT a FROM t INDEXED BY i WHERE a=1").is_ok());
    assert!(c.query("SELECT a FROM t NOT INDEXED WHERE a=1").is_ok());
    // An implicit UNIQUE/PK auto-index name is accepted.
    c.execute("CREATE TABLE u(k UNIQUE, v)").unwrap();
    c.execute("INSERT INTO u VALUES (1,2)").unwrap();
    assert!(
        c.query("SELECT v FROM u INDEXED BY sqlite_autoindex_u_1 WHERE k=1")
            .is_ok()
    );
}
