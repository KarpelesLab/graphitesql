//! Roadmap C8a: `PRAGMA secure_delete`. graphite used to hard-code the getter to
//! 0; now it round-trips the value like sqlite (0=off, 1=on, 2=`fast`) and, when
//! non-zero, zeroes the content of pages handed to the freelist so deleted data
//! does not linger on disk.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

/// What value `sqlite3` reports after `PRAGMA secure_delete=<set>`.
fn sqlite_value(set: &str) -> Option<i64> {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        return None;
    }
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("PRAGMA secure_delete={set}; PRAGMA secure_delete;"))
        .output()
        .unwrap();
    // The set form echoes the resolved value, then the getter repeats it.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .and_then(|l| l.trim().parse().ok())
}

#[test]
fn round_trips_like_sqlite() {
    for set in ["0", "1", "2", "3", "on", "off", "fast", "true", "false"] {
        let Some(want) = sqlite_value(set) else {
            eprintln!("sqlite3 not found; skipping");
            return;
        };
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("PRAGMA secure_delete={set}")).unwrap();
        let got = match c.query("PRAGMA secure_delete").unwrap().rows[0][0] {
            Value::Integer(n) => n,
            ref v => panic!("non-integer secure_delete: {v:?}"),
        };
        assert_eq!(got, want, "secure_delete={set}");
    }
}

#[test]
fn defaults_to_off() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        c.query("PRAGMA secure_delete").unwrap().rows[0][0],
        Value::Integer(0)
    );
}

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    (0..=haystack.len() - needle.len())
        .filter(|&i| &haystack[i..i + needle.len()] == needle)
        .count()
}

/// Build a file db whose single big-blob row spans several overflow pages, delete
/// it under the given `secure_delete` mode, then return how many copies of the
/// marker survive in the raw file (and assert the file stays integrity-clean).
fn surviving_markers(secure: bool) -> usize {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "gsql-secdel-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    let marker = "SECRETMARKERDATA";
    let blob = marker.repeat(2400); // ~37 KiB → several overflow pages
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, b TEXT)")
            .unwrap();
        if secure {
            c.execute("PRAGMA secure_delete=ON").unwrap();
        }
        c.execute(&format!("INSERT INTO t VALUES (1, '{blob}')"))
            .unwrap();
        c.execute("DELETE FROM t WHERE id=1").unwrap();
        assert_eq!(
            c.query("PRAGMA integrity_check").unwrap().rows[0][0],
            Value::Text("ok".into()),
            "integrity_check after delete"
        );
    } // drop flushes/closes
    let bytes = std::fs::read(&path).unwrap();
    let n = count_occurrences(&bytes, marker.as_bytes());
    let _ = std::fs::remove_file(&path);
    n
}

#[test]
fn secure_delete_zeroes_freed_pages() {
    // With secure_delete=ON no copy of the deleted blob survives on disk.
    assert_eq!(surviving_markers(true), 0, "ON should zero freed pages");
    // The default (OFF) leaves freed-page content behind — proof the setting
    // actually changes behavior (not that graphite always zeroes).
    assert!(
        surviving_markers(false) > 0,
        "OFF should leave freed content on disk"
    );
}
