//! Regression: robust *index* b-tree page splitting under heavy insert/delete/
//! update churn with large index keys.
//!
//! Before the fix, an over-full index leaf (or interior) page was split by a
//! single fixed midpoint halving (`m = len/2`) that did not guarantee each half
//! individually fit a page. When the large index-record cells clustered on one
//! side of the midpoint, that half exceeded the page and `serialize_index_leaf`
//! panicked with `attempt to subtract with overflow` (or wrote a page whose cell
//! pointers pointed past the page end, read back as `Corrupt`). This affects
//! secondary indexes (`CREATE INDEX`) and `WITHOUT ROWID` tables (stored as index
//! b-trees).
//!
//! These tests drive deterministic large-key churn and assert the file stays
//! valid under graphitesql's own `integrity_check`, under sqlite3's `quick_check`
//! (when available), and that the primary row set AND the index-ordered scan
//! match a sqlite3 run of the same deterministic script — no row or index entry
//! is ever lost, duplicated, or corrupted.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run `sqlite3 <db>` feeding `script` on stdin (scripts are far too large to
/// pass as a single argv element). Returns success.
fn sqlite3_replay(db: &str, script: &str) -> bool {
    let mut child = Command::new("sqlite3")
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    child.wait().unwrap().success()
}

fn graphite_check(c: &Connection) -> String {
    match &c.query("PRAGMA integrity_check").unwrap().rows[0][0] {
        Value::Text(s) => String::from(s.as_str()),
        _ => "?".into(),
    }
}

/// A deterministic value of `len` bytes derived from `tag`, containing no single
/// quote so it embeds directly in an SQL string literal.
fn value(tag: u64, len: usize) -> String {
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut s = String::with_capacity(len);
    let mut x = tag | 1;
    for _ in 0..len {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.push(alphabet[(x >> 33) as usize % alphabet.len()] as char);
    }
    s
}

/// Deterministic size mix, indexed by iteration.
fn size(sizes: &[usize], i: usize) -> usize {
    sizes[i % sizes.len()]
}

/// Replay the same deterministic SQL script through graphite and sqlite3, then
/// assert both accept the graphite-written file and every result query matches.
struct Parity {
    name: String,
    gpath: String,
    spath: String,
}

impl Parity {
    fn new(name: &str, script: &str) -> Self {
        let dir = std::env::temp_dir();
        let gpath = dir
            .join(format!("gsql-idxchurn-{name}-{}.g.db", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let spath = dir
            .join(format!("gsql-idxchurn-{name}-{}.s.db", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_file(&gpath);
        let _ = std::fs::remove_file(&spath);

        {
            let mut c = Connection::create(&gpath).unwrap();
            for stmt in script.split(";\n") {
                let s = stmt.trim();
                if !s.is_empty() {
                    c.execute(s).unwrap();
                }
            }
            assert_eq!(graphite_check(&c), "ok", "{name}: graphite integrity_check");
        }

        if sqlite3_available() {
            assert!(
                sqlite3_replay(&spath, script),
                "{name}: sqlite3 replay failed"
            );
            let out = Command::new("sqlite3")
                .arg(&gpath)
                .arg("PRAGMA quick_check;")
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim(),
                "ok",
                "{name}: sqlite3 quick_check on graphite file"
            );
        }

        Parity {
            name: name.into(),
            gpath,
            spath,
        }
    }

    /// Assert a query returns byte-identical output from graphite and sqlite3.
    fn assert_same(&self, query: &str) {
        if !sqlite3_available() {
            // Still exercise graphite so the query at least runs cleanly.
            let c = Connection::open(&self.gpath).unwrap();
            c.query(query).unwrap();
            return;
        }
        let g = Command::new("sqlite3")
            .arg(&self.gpath)
            .arg(query)
            .output()
            .unwrap();
        let s = Command::new("sqlite3")
            .arg(&self.spath)
            .arg(query)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&g.stdout),
            String::from_utf8_lossy(&s.stdout),
            "{}: mismatch for `{query}`",
            self.name
        );
    }
}

impl Drop for Parity {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.gpath);
        let _ = std::fs::remove_file(&self.spath);
    }
}

/// Rolling secondary-index churn with large keys: the exact workload that
/// panicked the old midpoint split (`index_writer.rs` `serialize_index_leaf`).
#[test]
fn secondary_index_large_key_churn() {
    let sizes = [20usize, 100, 400, 900, 1500, 2000, 50];
    let mut script = String::from("CREATE TABLE t(k INTEGER PRIMARY KEY, b TEXT);\n");
    script.push_str("CREATE INDEX ib ON t(b);\n");
    let mut live: std::collections::VecDeque<i64> = Default::default();
    for (it, nk) in (1i64..=1100).enumerate() {
        let sz = size(&sizes, it);
        let v = value(nk as u64, sz);
        script.push_str(&format!("INSERT INTO t VALUES({nk}, '{v}_{nk}');\n"));
        live.push_back(nk);
        while live.len() > 50 {
            let old = live.pop_front().unwrap();
            script.push_str(&format!("DELETE FROM t WHERE k={old};\n"));
        }
        if it % 4 == 0 && !live.is_empty() {
            let t = live[it % live.len()];
            let v = value((it as u64) << 8, size(&sizes, it + 3));
            script.push_str(&format!("UPDATE t SET b='{v}_{t}' WHERE k={t};\n"));
        }
    }
    let p = Parity::new("sec-large", &script);
    p.assert_same("SELECT count(*) FROM t;");
    p.assert_same("SELECT k, b FROM t ORDER BY k;");
    p.assert_same("SELECT k FROM t ORDER BY b, k;"); // index-ordered scan
}

/// `WITHOUT ROWID` table with large text PK keys, churned: the row store *is* an
/// index b-tree, so it exercises the same split path with record entries.
#[test]
fn without_rowid_large_key_churn() {
    let sizes = [16usize, 64, 300, 700, 1200, 2000, 900];
    let mut script = String::from("CREATE TABLE t(k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID;\n");
    let mut live: std::collections::VecDeque<String> = Default::default();
    for (it, nk) in (1i64..=900).enumerate() {
        let sz = size(&sizes, it);
        let key = format!("{nk:07}_{}", value(nk as u64, sz));
        script.push_str(&format!("INSERT INTO t VALUES('{key}', 'v{nk}');\n"));
        live.push_back(key);
        while live.len() > 40 {
            let old = live.pop_front().unwrap();
            script.push_str(&format!("DELETE FROM t WHERE k='{old}';\n"));
        }
        if it % 3 == 0 && !live.is_empty() {
            let k = live[it % live.len()].clone();
            script.push_str(&format!("UPDATE t SET v='u{it}' WHERE k='{k}';\n"));
        }
    }
    let p = Parity::new("wr-large", &script);
    p.assert_same("SELECT count(*) FROM t;");
    p.assert_same("SELECT k, v FROM t ORDER BY k;"); // PK (clustered index) order
    p.assert_same("SELECT v FROM t ORDER BY k;");
}

/// Two secondary indexes over wide (near-page) keys with an update-heavy mix:
/// stresses the skew (one large record cell dominating a leaf's byte total) that
/// left the old midpoint half over a page, across independent index b-trees.
#[test]
fn wide_key_multi_index_update_heavy() {
    let sizes = [2000usize, 1900, 2040, 1800, 60, 1000];
    let mut script = String::from("CREATE TABLE w(id INTEGER PRIMARY KEY, a TEXT, b TEXT);\n");
    script.push_str("CREATE INDEX iwa ON w(a);\n");
    script.push_str("CREATE INDEX iwb ON w(b);\n");
    let mut live: std::collections::VecDeque<i64> = Default::default();
    for (it, nk) in (1i64..=800).enumerate() {
        let a = value(nk as u64, size(&sizes, it));
        let b = value((nk as u64) ^ 0x55, size(&sizes, it + 2));
        script.push_str(&format!(
            "INSERT INTO w VALUES({nk}, '{a}_{nk}', '{b}_{nk}');\n"
        ));
        live.push_back(nk);
        while live.len() > 25 {
            let old = live.pop_front().unwrap();
            script.push_str(&format!("DELETE FROM w WHERE id={old};\n"));
        }
        if !live.is_empty() {
            let t = live[it % live.len()];
            let a = value((it as u64) << 4, size(&sizes, it + 1));
            script.push_str(&format!("UPDATE w SET a='{a}_{t}' WHERE id={t};\n"));
        }
    }
    let p = Parity::new("wide-multi", &script);
    p.assert_same("SELECT count(*) FROM w;");
    p.assert_same("SELECT id, a, b FROM w ORDER BY id;");
    p.assert_same("SELECT id FROM w ORDER BY a, id;");
    p.assert_same("SELECT id FROM w ORDER BY b, id;");
}

/// Bulk-load a large index (no deletes) so the index b-tree grows several levels
/// deep with big keys — forces interior-page splits carrying large record
/// separators. Verifies exact preservation via graphite's own read path too.
#[test]
fn secondary_index_bulk_large_keys() {
    let sizes = [900usize, 1500, 2000, 2040, 300];
    let mut script = String::from("CREATE TABLE t(k INTEGER PRIMARY KEY, b TEXT);\n");
    script.push_str("CREATE INDEX ib ON t(b);\n");
    let mut expected: BTreeMap<i64, String> = BTreeMap::new();
    for k in 1..=900i64 {
        let sz = size(&sizes, k as usize);
        let v = format!("{}_{k}", value(k as u64, sz));
        script.push_str(&format!("INSERT INTO t VALUES({k}, '{v}');\n"));
        expected.insert(k, v);
    }
    let p = Parity::new("bulk-large", &script);
    p.assert_same("SELECT count(*) FROM t;");
    p.assert_same("SELECT k FROM t ORDER BY b, k;");

    // Independent verification via graphite's own reader.
    let c = Connection::open(&p.gpath).unwrap();
    let res = c.query("SELECT k, b FROM t ORDER BY k").unwrap();
    assert_eq!(res.rows.len(), expected.len());
    for (row, (k, v)) in res.rows.iter().zip(expected.iter()) {
        let gk = match &row[0] {
            Value::Integer(i) => *i,
            other => panic!("k not integer: {other:?}"),
        };
        let gv = match &row[1] {
            Value::Text(s) => String::from(s.as_str()),
            other => panic!("b unexpected: {other:?}"),
        };
        assert_eq!(gk, *k);
        assert_eq!(&gv, v, "value mismatch for k={k}");
    }
}
