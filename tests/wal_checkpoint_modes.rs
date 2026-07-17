//! `PRAGMA wal_checkpoint(PASSIVE|FULL|RESTART|TRUNCATE)` semantics — the port
//! of `wal.c`'s `sqlite3WalCheckpoint`/`walCheckpoint` — plus the WAL
//! byte-stream structure that goes with them (`walRestartHdr`/`walRestartLog`):
//!
//! * the `(busy, log, checkpointed)` triples match what a pinned `sqlite3
//!   3.50.4` reports for the same scenarios (verified against the live CLI in
//!   this repo's environment);
//! * PASSIVE/FULL leave the `-wal` bytes in place; TRUNCATE zeroes the file and
//!   reports `(0, 0, 0)`;
//! * after a full checkpoint the **next writer restarts the log**: the header
//!   is rewritten in place with an incremented checkpoint sequence (bytes
//!   12–15) and salt-1 bumped by one as a big-endian integer, and the frames
//!   overwrite the file from offset 32 without truncating it — after which the
//!   real `sqlite3` still opens the database mid-WAL, sees exactly the
//!   committed data, and `integrity_check` is `ok` (the stale frames beyond the
//!   restart fail the salt check, for sqlite and for graphite alike).

#![cfg(feature = "std")]

use graphitesql::pager::{CheckpointMode, PageSource, WritePager};
use graphitesql::vfs::std_file::StdVfs;
use graphitesql::vfs::{OpenFlags, Vfs};
use std::process::Command;

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-ckptm-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

fn cleanup(path: &str) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{path}{suffix}"));
    }
}

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite3_run(path: &str, sql: &str) -> String {
    let out = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Open (creating on first use) a WAL-mode pager over `path`.
fn wal_pager(path: &str, create: bool) -> WritePager {
    let vfs = StdVfs::new();
    let main = vfs.open(path, OpenFlags::READ_WRITE_CREATE).unwrap();
    let journal = vfs
        .open(&format!("{path}-journal"), OpenFlags::READ_WRITE_CREATE)
        .unwrap();
    let wal = vfs
        .open(&format!("{path}-wal"), OpenFlags::READ_WRITE_CREATE)
        .unwrap();
    let mut wp = if create {
        let mut wp = WritePager::create_wal(main, Some(journal), Some(wal), 4096).unwrap();
        wp.commit().unwrap();
        wp
    } else {
        WritePager::open_wal(main, Some(journal), Some(wal)).unwrap()
    };
    assert!(wp.set_wal_mode().unwrap());
    wp
}

/// Commit one single-frame transaction: bump `user_version` to `v` (page 1 is
/// the only dirty page, so each call appends exactly one commit frame).
fn commit_uv(wp: &mut WritePager, v: u32) {
    wp.header_mut().user_version = v;
    wp.commit().unwrap();
}

fn wal_size(path: &str) -> u64 {
    std::fs::metadata(format!("{path}-wal")).unwrap().len()
}

/// `(checkpoint_sequence, salt1)` from the on-disk WAL header.
fn wal_hdr_seq_salt1(path: &str) -> (u32, u32) {
    let bytes = std::fs::read(format!("{path}-wal")).unwrap();
    let be32 =
        |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    (be32(12), be32(16))
}

/// One frame is 24 bytes of header plus a 4096-byte page.
const FRAME: u64 = 24 + 4096;

/// PASSIVE with nothing blocking: everything is backfilled, the triple is
/// `(0, n, n)` (sqlite: `0|3|3`), and the `-wal` bytes are left in place —
/// after which the real sqlite3 still reads the database.
#[test]
fn passive_backfills_all_and_keeps_wal() {
    let path = temp_path("passive.db");
    cleanup(&path);
    let mut wp = wal_pager(&path, true);
    for v in 1..=3 {
        commit_uv(&mut wp, v);
    }
    let before = wal_size(&path);
    assert_eq!(before, 32 + 3 * FRAME);

    assert_eq!(
        wp.checkpoint_mode(CheckpointMode::Passive).unwrap(),
        (0, 3, 3)
    );
    assert_eq!(wal_size(&path), before, "PASSIVE must not touch the -wal");
    // Idempotent: nothing left to do.
    assert_eq!(
        wp.checkpoint_mode(CheckpointMode::Passive).unwrap(),
        (0, 3, 3)
    );
    drop(wp);

    if sqlite3_available() {
        assert_eq!(sqlite3_run(&path, "PRAGMA integrity_check;"), "ok");
        assert_eq!(sqlite3_run(&path, "PRAGMA user_version;"), "3");
    }
    cleanup(&path);
}

/// TRUNCATE zeroes the `-wal` and reports `(0, 0, 0)` (sqlite: `0|0|0`).
#[test]
fn truncate_zeroes_wal() {
    let path = temp_path("truncate.db");
    cleanup(&path);
    let mut wp = wal_pager(&path, true);
    for v in 1..=2 {
        commit_uv(&mut wp, v);
    }
    assert_eq!(
        wp.checkpoint_mode(CheckpointMode::Truncate).unwrap(),
        (0, 0, 0)
    );
    assert_eq!(wal_size(&path), 0, "TRUNCATE zeroes the -wal file");
    drop(wp);

    if sqlite3_available() {
        assert_eq!(sqlite3_run(&path, "PRAGMA integrity_check;"), "ok");
        assert_eq!(sqlite3_run(&path, "PRAGMA user_version;"), "2");
    }
    cleanup(&path);
}

/// A reader pinned below the log's end limits the backfill. PASSIVE reports
/// `(0, n, mark)` without complaint; FULL/RESTART/TRUNCATE report busy and do
/// not touch the `-wal` (sqlite: `0|1|0` then `1|1|0` in the same scenario).
/// Once the reader ends, FULL completes with `(0, n, n)`.
#[test]
fn pinned_reader_limits_backfill_and_flags_busy() {
    let path = temp_path("pinned.db");
    cleanup(&path);
    let mut writer = wal_pager(&path, true);
    for v in 1..=2 {
        commit_uv(&mut writer, v);
    }
    // A sibling connection pins a read transaction at the current mark (2)…
    let reader = wal_pager(&path, false);
    reader.begin_read_txn().unwrap();
    // …then the writer commits one more frame past the pin.
    commit_uv(&mut writer, 3);
    let before = wal_size(&path);

    assert_eq!(
        writer.checkpoint_mode(CheckpointMode::Passive).unwrap(),
        (0, 3, 2),
        "PASSIVE: backfill stops at the pinned mark, no busy"
    );
    assert_eq!(
        writer.checkpoint_mode(CheckpointMode::Full).unwrap(),
        (1, 3, 2),
        "FULL: the pinned reader makes the checkpoint busy"
    );
    assert_eq!(
        writer.checkpoint_mode(CheckpointMode::Truncate).unwrap(),
        (1, 3, 2),
        "TRUNCATE: blocked the same way"
    );
    assert_eq!(wal_size(&path), before, "a blocked TRUNCATE keeps the -wal");

    reader.end_read_txn();
    assert_eq!(
        writer.checkpoint_mode(CheckpointMode::Full).unwrap(),
        (0, 3, 3),
        "reader gone: FULL completes"
    );
    drop(reader);
    drop(writer);
    cleanup(&path);
}

/// RESTART reports `(0, n, n)` and leaves the `-wal` bytes alone; the **next**
/// commit then restarts the log in place: same file size, checkpoint sequence
/// +1, salt-1 +1 (big-endian), frames rewritten from offset 32 — and both the
/// real sqlite3 and a fresh graphite open read exactly the post-restart state
/// through the stale tail.
#[test]
fn restart_then_next_commit_overwrites_in_place() {
    let path = temp_path("restart.db");
    cleanup(&path);
    let mut wp = wal_pager(&path, true);
    for v in 1..=3 {
        commit_uv(&mut wp, v);
    }
    let size3 = wal_size(&path);
    let (seq0, salt1_0) = wal_hdr_seq_salt1(&path);

    assert_eq!(
        wp.checkpoint_mode(CheckpointMode::Restart).unwrap(),
        (0, 3, 3)
    );
    assert_eq!(
        wal_size(&path),
        size3,
        "RESTART leaves the -wal bytes alone"
    );
    assert_eq!(wal_hdr_seq_salt1(&path), (seq0, salt1_0));

    // The next commit restarts the log from the beginning (walRestartLog).
    commit_uv(&mut wp, 4);
    assert_eq!(
        wal_size(&path),
        size3,
        "the restarted log overwrites in place — no truncate, no append"
    );
    let (seq1, salt1_1) = wal_hdr_seq_salt1(&path);
    assert_eq!(seq1, seq0 + 1, "checkpoint sequence increments on restart");
    assert_eq!(salt1_1, salt1_0.wrapping_add(1), "salt-1 increments by one");

    // A fresh graphite open of the on-disk state sees only the new generation.
    drop(wp);
    let wp = wal_pager(&path, false);
    assert_eq!(wp.header().user_version, 4);
    drop(wp);

    // And so does the real sqlite3, mid-WAL, with a clean integrity check.
    if sqlite3_available() {
        assert_eq!(sqlite3_run(&path, "PRAGMA integrity_check;"), "ok");
        assert_eq!(sqlite3_run(&path, "PRAGMA user_version;"), "4");
    }
    cleanup(&path);
}

/// On a rollback-journal database the triple is `(0, -1, -1)`, like sqlite's
/// `PRAGMA wal_checkpoint` on a non-WAL database.
#[test]
fn non_wal_database_reports_minus_one() {
    let path = temp_path("nonwal.db");
    cleanup(&path);
    let vfs = StdVfs::new();
    let main = vfs.open(&path, OpenFlags::READ_WRITE_CREATE).unwrap();
    let mut wp = WritePager::create(main, None, 4096).unwrap();
    wp.commit().unwrap();
    assert_eq!(
        wp.checkpoint_mode(CheckpointMode::Passive).unwrap(),
        (0, -1, -1)
    );
    assert_eq!(
        wp.checkpoint_mode(CheckpointMode::Truncate).unwrap(),
        (0, -1, -1)
    );
    drop(wp);
    cleanup(&path);
}

/// `CheckpointMode::from_name` maps like `pragma.c`: full/restart/truncate,
/// case-insensitively; anything else is PASSIVE.
#[test]
fn mode_names_parse_like_sqlite() {
    assert_eq!(CheckpointMode::from_name("FULL"), CheckpointMode::Full);
    assert_eq!(CheckpointMode::from_name("full"), CheckpointMode::Full);
    assert_eq!(
        CheckpointMode::from_name("Restart"),
        CheckpointMode::Restart
    );
    assert_eq!(
        CheckpointMode::from_name("truncate"),
        CheckpointMode::Truncate
    );
    assert_eq!(
        CheckpointMode::from_name("passive"),
        CheckpointMode::Passive
    );
    assert_eq!(CheckpointMode::from_name("bogus"), CheckpointMode::Passive);
    assert_eq!(CheckpointMode::from_name("0"), CheckpointMode::Passive);
}
