//! The SQLite `base85` extension's byte<->text conversion — a faithful port of
//! `ext/misc/base85.c` (3.50.4). `base85(X)` toggles on the argument type: a
//! BLOB is encoded to base85 TEXT (groups of 4 bytes → 5 numerals, lines
//! wrapped at 80 numerals with `\n`, output `\n`-terminated); TEXT is decoded
//! back to a BLOB (non-base85 bytes delimit groups, so whitespace-separated
//! sequences concatenate). The numeral set, group arithmetic and lenient
//! decoding mirror the C exactly so results byte-match sqlite.
//!
//! Base85 numerals are the 85 ASCII codes `#`, `$`, `%`, `&` (digit values
//! 0..3) followed by `*` … `z` contiguously (digit values 4..84).

use alloc::string::String;
use alloc::vec::Vec;

/// Width of base85 output lines (`B85_DARK_MAX`), a multiple of 5.
const B85_DARK_MAX: i32 = 80;

/// `base85Numeral`: map a digit value 0..84 to its ASCII numeral.
#[inline]
fn numeral(dn: u8) -> u8 {
    if dn < 4 { dn + b'#' } else { dn - 4 + b'*' }
}

/// `B85_CLASS`: `(c>='#') + (c>'&') + (c>='*') + (c>'z')`. Odd values (1, 3)
/// denote a base85 numeral.
#[inline]
fn b85_class(c: u8) -> usize {
    (c >= b'#') as usize + (c > b'&') as usize + (c >= b'*') as usize + (c > b'z') as usize
}

/// `IS_B85`: whether `c` is a base85 numeral.
#[inline]
fn is_b85(c: u8) -> bool {
    b85_class(c) & 1 != 0
}

/// `B85_DNOS`: the offset subtracted from a numeral to recover its digit value
/// (0 for a non-numeral), via `b85_cOffset[B85_CLASS(c)]`.
#[inline]
fn b85_dnos(c: u8) -> u8 {
    // b85_cOffset = { 0, '#', 0, '*'-4, 0 }
    const OFF: [u8; 5] = [0, b'#', 0, b'*' - 4, 0];
    OFF[b85_class(c)]
}

/// Encode a byte buffer into base85 text, ported from `toBase85` with the
/// separator `"\n"`: a newline caps every `B85_DARK_MAX` numerals and
/// terminates the final group (so the output always ends in `\n` when non-empty).
pub fn encode(input: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(input.len() * 5 / 4 + input.len() / 64 + 4);
    let mut n_col: i32 = 0;
    let mut p_in = input;
    while p_in.len() >= 4 {
        let mut qbv: u32 = ((p_in[0] as u32) << 24)
            | ((p_in[1] as u32) << 16)
            | ((p_in[2] as u32) << 8)
            | (p_in[3] as u32);
        let mut quad = [0u8; 5];
        let mut nco = 5usize;
        while nco > 0 {
            let nqv = qbv / 85;
            let dv = (qbv - 85 * nqv) as u8;
            qbv = nqv;
            nco -= 1;
            quad[nco] = numeral(dv);
        }
        out.extend_from_slice(&quad);
        p_in = &p_in[4..];
        n_col += 5;
        if n_col >= B85_DARK_MAX {
            out.push(b'\n');
            n_col = 0;
        }
    }
    let nb_in = p_in.len();
    if nb_in > 0 {
        let nco0 = nb_in + 1;
        // Pack the 1..3 remaining bytes big-endian (C: `nbe=1; while(nbe++<nbIn)`).
        let mut qv: u64 = p_in[0] as u64;
        let mut idx = 1usize;
        let mut nbe = 1usize;
        while {
            let cont = nbe < nb_in;
            nbe += 1;
            cont
        } {
            qv = (qv << 8) | p_in[idx] as u64;
            idx += 1;
        }
        n_col += nco0 as i32;
        let mut tail = alloc::vec![0u8; nco0];
        let mut nco = nco0;
        while nco > 0 {
            let dv = (qv % 85) as u8;
            qv /= 85;
            nco -= 1;
            tail[nco] = numeral(dv);
        }
        out.extend_from_slice(&tail);
    }
    if n_col > 0 {
        out.push(b'\n');
    }
    // Output is pure ASCII by construction.
    String::from_utf8(out).unwrap_or_default()
}

/// Decode base85 text into a byte buffer, ported from `fromBase85`: leading
/// non-numerals are skipped (so groups may be whitespace/newline separated), a
/// non-numeral mid-group terminates that group early, and a trailing `\n` is
/// ignored.
pub fn decode(input: &[u8]) -> Vec<u8> {
    const NBOI: [i32; 6] = [0, 0, 1, 2, 3, 4];
    let mut out = Vec::with_capacity(input.len() * 4 / 5 + 4);
    let mut nc_in = input.len();
    if nc_in > 0 && input[nc_in - 1] == b'\n' {
        nc_in -= 1;
    }
    let mut pos = 0usize;
    while nc_in > 0 {
        // skipNonB85: advance over non-numeral bytes (also stops at a NUL byte).
        while nc_in > 0 && input[pos] != 0 && !is_b85(input[pos]) {
            pos += 1;
            nc_in -= 1;
        }
        if nc_in == 0 {
            break;
        }
        let mut nti = if nc_in > 5 { 5 } else { nc_in } as i32;
        let mut nbo = NBOI[nti as usize];
        if nbo == 0 {
            break;
        }
        let mut qv: u64 = 0;
        while nti > 0 {
            let c = input[pos];
            pos += 1;
            nc_in -= 1;
            let cdo = b85_dnos(c);
            if cdo == 0 {
                break;
            }
            qv = 85 * qv + (c - cdo) as u64;
            nti -= 1;
        }
        nbo -= nti; // adjust for an early (non-digit) end of group
        if nbo >= 4 {
            out.push((qv >> 24) as u8);
        }
        if nbo >= 3 {
            out.push((qv >> 16) as u8);
        }
        if nbo >= 2 {
            out.push((qv >> 8) as u8);
        }
        if nbo >= 1 {
            out.push(qv as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for len in 0..40usize {
            let msg: Vec<u8> = (0..len).map(|i| (i * 7 + 1) as u8).collect();
            let enc = encode(&msg);
            assert_eq!(decode(enc.as_bytes()), msg, "len {len}");
        }
    }

    #[test]
    fn empty() {
        assert_eq!(encode(b""), "");
        assert_eq!(decode(b""), b"");
    }

    #[test]
    fn wraps_at_80() {
        // 70 zero bytes → 88 numerals, wrapped 80 + '\n' + 8 + '\n'.
        let enc = encode(&[0u8; 70]);
        let lines: Vec<&str> = enc.trim_end().split('\n').collect();
        assert_eq!(lines[0].len(), 80);
        assert_eq!(lines[1].len(), 8);
        assert!(enc.ends_with('\n'));
    }

    #[test]
    fn decode_skips_separators() {
        // A '\n'-separated two-group encoding round-trips.
        let msg = [1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(decode(encode(&msg).as_bytes()), msg);
    }
}
