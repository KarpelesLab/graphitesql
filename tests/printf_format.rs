//! `printf`/`format` flags that graphite recently gained — the `,`
//! thousands-grouping flag and the `l`/`ll` length modifiers — matched against
//! the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn graphite(expr: &str) -> String {
    let c = Connection::open_memory().unwrap();
    match &c.query(&format!("SELECT {expr}")).unwrap().rows[0][0] {
        graphitesql::Value::Text(t) => String::from(t.as_str()),
        other => format!("{other:?}"),
    }
}

#[test]
fn comma_flag_and_length_modifiers_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let exprs = [
        "printf('%,d', 1234567)",
        "printf('%,d', -1234567)",
        "printf('%,d', 100)",
        "printf('%,d', 1000)",
        "printf('%,d', 0)",
        "printf('%,.2f', 1234567.89)",
        "printf('%,15d', 1234567)",
        "printf('%,x', 255)", // grouping does not apply to hex
        "printf('%ld', 42)",
        "printf('%lld', 999)",
        "printf('%,lld', 1234567890123)",
        "printf('%d', 1234)", // no comma: unchanged
    ];
    for e in exprs {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(format!("SELECT {e};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        assert_eq!(graphite(e), want, "diverged on {e}");
    }
}
