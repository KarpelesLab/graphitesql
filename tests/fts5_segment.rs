//! Roadmap D2e-M2: byte-exact FTS5 `%_data` segment encoding.
//!
//! graphite currently READS fts5 (D2e-M1) from the `%_content` documents and
//! stores its own documents in a generic `<name>_data` backing table — so a
//! graphite-written fts5 table is not yet `MATCH`-able by stock sqlite, which
//! expects the segmented `%_data`/`%_idx` inverted index. Closing that gap is a
//! storage-layer rework (swap the generic backing table for sqlite's five shadow
//! tables). Its core risk is reproducing sqlite's segment bytes exactly.
//!
//! This test retires that risk: it builds a single-leaf segment (the structure
//! record, the averages record, and the leaf page) in Rust and proves the bytes
//! are identical to what `sqlite3` 3.50.4 writes, across several doclist shapes
//! (multiple docs per term → rowid deltas, multiple positions per doc → collist
//! deltas, term prefix compression including the `FTS5_MAIN_PREFIX` '0' byte).
//! The encoder here is the reference the storage rework will port into the lib.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use std::process::Command;

/// sqlite's varint (the standard b-tree varint): 7 bits per byte, big-endian,
/// high bit = "more"; the 9th byte (only for values using > 56 bits) holds a full
/// 8 bits. All values encoded by this test fit well under that threshold.
fn put_varint(out: &mut Vec<u8>, v: u64) {
    if v >> 56 != 0 {
        // 9-byte form: eight 7-bit groups (continuation set) then a full byte.
        for i in (0..8).rev() {
            out.push((((v >> (i * 7 + 8)) & 0x7f) as u8) | 0x80);
        }
        out.push((v & 0xff) as u8);
        return;
    }
    // 1..=8 byte form: emit 7-bit groups most-significant first, continuation bit
    // on all but the last.
    let mut groups = [0u8; 9];
    let mut n = 0;
    let mut w = v;
    loop {
        groups[n] = (w & 0x7f) as u8;
        n += 1;
        w >>= 7;
        if w == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        let byte = groups[i] | if i != 0 { 0x80 } else { 0 };
        out.push(byte);
    }
}

/// A document's contribution to one term: its rowid and the sorted token
/// positions within column 0.
struct Posting {
    rowid: i64,
    positions: Vec<u32>,
}

/// Encode the position list (column 0 only) for one posting: size header
/// (collist bytes * 2) then the collist (`first+2`, then `delta+2`).
fn encode_poslist(p: &Posting) -> Vec<u8> {
    let mut collist = Vec::new();
    let mut prev = 0u32;
    for (i, &pos) in p.positions.iter().enumerate() {
        let delta = if i == 0 { pos } else { pos - prev };
        put_varint(&mut collist, (delta as u64) + 2);
        prev = pos;
    }
    let mut out = Vec::new();
    put_varint(&mut out, (collist.len() as u64) * 2);
    out.extend_from_slice(&collist);
    out
}

/// Encode a term's doclist: first rowid, then `(rowidDelta, poslist)` per doc.
fn encode_doclist(postings: &[Posting]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0i64;
    for (i, p) in postings.iter().enumerate() {
        if i == 0 {
            put_varint(&mut out, p.rowid as u64);
        } else {
            put_varint(&mut out, (p.rowid - prev) as u64);
        }
        out.extend_from_slice(&encode_poslist(p));
        prev = p.rowid;
    }
    out
}

/// Build a single leaf page from the sorted term list. Each term is the raw token
/// (the `FTS5_MAIN_PREFIX` '0' is prepended here). Terms must be sorted ascending.
fn encode_leaf(terms: &[(String, Vec<Posting>)]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut offsets = Vec::new();
    let mut prev_key: Vec<u8> = Vec::new();
    for (term, postings) in terms {
        offsets.push(4 + body.len()); // page-relative offset of this term record
        let mut key = Vec::with_capacity(term.len() + 1);
        key.push(b'0'); // FTS5_MAIN_PREFIX
        key.extend_from_slice(term.as_bytes());
        if prev_key.is_empty() {
            // First term on the page: size varint + the full key blob.
            put_varint(&mut body, key.len() as u64);
            body.extend_from_slice(&key);
        } else {
            // Prefix-compressed: nCommon, nNew, new suffix bytes.
            let n_common = key
                .iter()
                .zip(prev_key.iter())
                .take_while(|(a, b)| a == b)
                .count();
            put_varint(&mut body, n_common as u64);
            put_varint(&mut body, (key.len() - n_common) as u64);
            body.extend_from_slice(&key[n_common..]);
        }
        body.extend_from_slice(&encode_doclist(postings));
        prev_key = key;
    }
    let footer_off = 4 + body.len();
    // pgidx: first term's absolute offset, then deltas.
    let mut pgidx = Vec::new();
    let mut prev_off = 0usize;
    for (i, &off) in offsets.iter().enumerate() {
        put_varint(
            &mut pgidx,
            (if i == 0 { off } else { off - prev_off }) as u64,
        );
        prev_off = off;
    }
    let mut leaf = Vec::new();
    // 4-byte header: two big-endian u16s. The first rowid is inside the first
    // term's doclist, so it never precedes the first term → field 0.
    leaf.extend_from_slice(&0u16.to_be_bytes());
    leaf.extend_from_slice(&(footer_off as u16).to_be_bytes());
    leaf.extend_from_slice(&body);
    leaf.extend_from_slice(&pgidx);
    leaf
}

/// The averages record (id=1): `[nRow, per-column total token count]`.
fn encode_averages(n_row: u64, total_tokens_col0: u64) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, n_row);
    put_varint(&mut out, total_tokens_col0);
    out
}

/// The structure record (id=10) for one fresh segment: 4-byte cookie, then
/// nLevel, nSegment, nWriteCounter, then per level nMerge/nSeg, then per segment
/// segid/pgnoFirst/pgnoLast.
fn encode_structure() -> Vec<u8> {
    let mut out = vec![0, 0, 0, 0]; // configuration cookie 0 (not a V2 record)
    for v in [1u64, 1, 1, /*level0:*/ 0, 1, /*seg:*/ 1, 1, 1] {
        put_varint(&mut out, v);
    }
    out
}

const SEG_LEAF_ROWID: i64 = (1i64 << 37) | 1; // segid=1, height=0, dli=0, pgno=1

/// Parse one `X'..'` hex blob literal as printed by `sqlite3`.
fn parse_hex(s: &str) -> Vec<u8> {
    let s = s.trim();
    let inner = s
        .strip_prefix("X'")
        .or_else(|| s.strip_prefix("x'"))
        .and_then(|r| r.strip_suffix('\''))
        .unwrap_or("");
    (0..inner.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&inner[i..i + 2], 16).unwrap())
        .collect()
}

/// Build `body`-column docs in a fresh fts5 table via sqlite3 (single INSERT
/// statement → one segment, one leaf for small inputs) and return the three
/// `%_data` rows (averages id=1, structure id=10, leaf SEG_LEAF_ROWID) as bytes.
fn sqlite_data_rows(docs: &[(i64, &str)]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "gsql-fts5seg-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let values: Vec<String> = docs.iter().map(|(r, b)| format!("({r},'{b}')")).collect();
    let script = format!(
        "CREATE VIRTUAL TABLE t USING fts5(body);\
         INSERT INTO t(rowid,body) VALUES {};",
        values.join(",")
    );
    let o = Command::new("sqlite3")
        .arg(&path)
        .arg(&script)
        .output()
        .unwrap();
    assert!(o.status.success(), "sqlite build failed: {o:?}");
    let fetch = |id: i64| {
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg(format!("SELECT quote(block) FROM t_data WHERE id={id};"))
            .output()
            .unwrap();
        parse_hex(&String::from_utf8_lossy(&o.stdout))
    };
    let r = (fetch(1), fetch(10), fetch(SEG_LEAF_ROWID));
    let _ = std::fs::remove_file(&path);
    r
}

/// Tokenize like the default unicode61 tokenizer for our simple lowercase-ascii
/// inputs: split on spaces. Build the sorted term → postings index.
fn build_index(docs: &[(i64, &str)]) -> (Vec<(String, Vec<Posting>)>, u64, u64) {
    use std::collections::BTreeMap;
    // term -> (rowid -> positions)
    let mut idx: BTreeMap<String, BTreeMap<i64, Vec<u32>>> = BTreeMap::new();
    let mut total_tokens = 0u64;
    for (rowid, body) in docs {
        for (pos, tok) in body.split_whitespace().enumerate() {
            total_tokens += 1;
            idx.entry(tok.to_string())
                .or_default()
                .entry(*rowid)
                .or_default()
                .push(pos as u32);
        }
    }
    let terms = idx
        .into_iter()
        .map(|(term, docs)| {
            let postings = docs
                .into_iter()
                .map(|(rowid, positions)| Posting { rowid, positions })
                .collect();
            (term, postings)
        })
        .collect();
    (terms, docs.len() as u64, total_tokens)
}

fn check(docs: &[(i64, &str)]) {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let (avg, structure, leaf) = sqlite_data_rows(docs);
    let (terms, n_row, total_tokens) = build_index(docs);
    assert_eq!(
        encode_averages(n_row, total_tokens),
        avg,
        "averages mismatch"
    );
    assert_eq!(encode_structure(), structure, "structure mismatch");
    assert_eq!(encode_leaf(&terms), leaf, "leaf mismatch for {docs:?}");
}

#[test]
fn single_term_single_doc() {
    check(&[(1, "a")]);
    check(&[(5, "cat")]);
}

#[test]
fn two_terms_prefix_compressed() {
    check(&[(1, "a b")]);
    check(&[(1, "apple apply")]); // shares "0appl" → nCommon 5
}

#[test]
fn multi_doc_rowid_deltas() {
    check(&[(1, "hello"), (2, "hello")]);
    check(&[(1, "x"), (3, "x")]); // rowid delta 2
}

#[test]
fn multi_position_collist() {
    check(&[(1, "the the")]); // one term, two positions
}

#[test]
fn mixed_three_terms() {
    check(&[(1, "red green"), (2, "green blue")]);
}
