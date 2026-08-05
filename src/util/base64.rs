//! The SQLite `base64` extension's byte<->text conversion — a faithful port of
//! `ext/misc/base64.c` (3.50.4). `base64(X)` toggles on the argument type:
//! a BLOB is encoded to base64 TEXT (lines wrapped at 72 dark characters, each
//! line and the whole output terminated by `\n`); TEXT is decoded back to a
//! BLOB (whitespace and non-base64 bytes are skipped, `=` pads terminate a
//! group). The bit-shuffling, line wrapping and lenient decoding mirror the C
//! exactly so the results byte-match sqlite.

use alloc::string::String;
use alloc::vec::Vec;

const B64_NUMERALS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Width of base64 output lines (`B64_DARK_MAX`), a multiple of 4.
const B64_DARK_MAX: i32 = 72;

// Decode-table sentinels (values ≥ 0x80 are "not a digit").
const PC: u8 = 0x80; // pad character `=`
const WS: u8 = 0x81; // whitespace
const ND: u8 = 0x82; // neither a digit nor the above

/// ASCII → base64 digit value (0..63) or a `PC`/`WS`/`ND` sentinel, mirroring
/// `b64DigitValues[128]`.
static B64_DIGIT_VALUES: [u8; 128] = [
    ND, ND, ND, ND, ND, ND, ND, ND, ND, WS, WS, WS, WS, WS, ND, ND, // 0x00
    ND, ND, ND, ND, ND, ND, ND, ND, ND, ND, ND, ND, ND, ND, ND, ND, // 0x10
    WS, ND, ND, ND, ND, ND, ND, ND, ND, ND, ND, 62, ND, ND, ND, 63, // 0x20 (sp..+../)
    52, 53, 54, 55, 56, 57, 58, 59, 60, 61, ND, ND, ND, PC, ND, ND, // 0x30 (0..9..=)
    ND, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, // 0x40 (A..O)
    15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, ND, ND, ND, ND, ND, // 0x50 (P..Z)
    ND, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, // 0x60 (a..o)
    41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, ND, ND, ND, ND, ND, // 0x70 (p..z)
];

/// Digit value of a byte (`BX_DV_PROTO`): the table value for ASCII, else `PC`
/// (a high byte is treated as pad, exactly like the C's `0x80`).
#[inline]
fn digit_value(c: u8) -> u8 {
    if c < 0x80 {
        B64_DIGIT_VALUES[c as usize]
    } else {
        PC
    }
}

#[inline]
fn is_digit(bdp: u8) -> bool {
    bdp < 0x80
}

/// Encode a byte buffer into base64 text, ported from `toBase64`. Line breaks
/// are inserted every `B64_DARK_MAX` dark characters and the output always ends
/// with a `\n` (the NUL terminator the C appends is not part of the string).
pub fn encode(input: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(input.len() * 4 / 3 + input.len() / 54 + 4);
    let mut n_col: i32 = 0;
    let mut p_in = input;
    while p_in.len() >= 3 {
        out.push(B64_NUMERALS[(p_in[0] >> 2) as usize]);
        out.push(B64_NUMERALS[(((p_in[0] << 4) | (p_in[1] >> 4)) & 0x3f) as usize]);
        out.push(B64_NUMERALS[(((p_in[1] & 0xf) << 2) | (p_in[2] >> 6)) as usize]);
        out.push(B64_NUMERALS[(p_in[2] & 0x3f) as usize]);
        p_in = &p_in[3..];
        n_col += 4;
        if n_col >= B64_DARK_MAX || p_in.is_empty() {
            out.push(b'\n');
            n_col = 0;
        }
    }
    let nb_in = p_in.len();
    if nb_in > 0 {
        let nco = (nb_in + 1) as i32;
        // Pack the remaining 1 or 2 bytes into the high end of a 24-bit group.
        let mut qv: u32 = p_in[0] as u32;
        let mut idx = 1usize;
        for nbe in 1..3 {
            qv <<= 8;
            if nbe < nb_in {
                qv |= p_in[idx] as u32;
                idx += 1;
            }
        }
        let mut quad = [0u8; 4];
        for nbe in (0..4).rev() {
            quad[nbe as usize] = if nbe < nco {
                B64_NUMERALS[(qv & 0x3f) as usize]
            } else {
                b'='
            };
            qv >>= 6;
        }
        out.extend_from_slice(&quad);
        out.push(b'\n');
    }
    // The output is pure ASCII by construction.
    String::from_utf8(out).unwrap_or_default()
}

/// Skip leading bytes that are not base64 digits (`skipNonB64`): returns the
/// index of the first digit within `s[..nc]`, or `nc` if none.
fn skip_non_b64(s: &[u8], nc: usize) -> usize {
    let mut i = 0;
    while i < nc && s[i] != 0 && !is_digit(digit_value(s[i])) {
        i += 1;
    }
    i
}

/// Decode base64 text into a byte buffer, ported from `fromBase64` (the lenient
/// SQLite decoder: whitespace/non-digits are skipped, a `=` pad or dark
/// non-digit terminates a group, a trailing `\n` is ignored).
pub fn decode(input: &[u8]) -> Vec<u8> {
    const NBOI: [i32; 5] = [0, 0, 1, 2, 3];
    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 3);
    // Drop a single trailing newline, as the C does.
    let mut nc_in = input.len();
    if nc_in > 0 && input[nc_in - 1] == b'\n' {
        nc_in -= 1;
    }
    // Reproduce the C's `pIn` cursor as an absolute index; the trailing-newline
    // trim above shortened `nc_in` only, so the cursor still starts at 0.
    let mut pos = 0usize;
    while nc_in > 0 && input[pos] != b'=' {
        let skipped = skip_non_b64(&input[pos..], nc_in);
        nc_in -= skipped;
        pos += skipped;
        let nti = if nc_in > 4 { 4 } else { nc_in };
        nc_in -= nti;
        let mut nbo = NBOI[nti];
        if nbo == 0 {
            break;
        }
        let mut qv: u32 = 0;
        let mut nti_eff = nti;
        for nac in 0..4 {
            let c = if nac < nti_eff {
                let ch = input[pos];
                pos += 1;
                ch
            } else {
                B64_NUMERALS[0]
            };
            let mut bdp = digit_value(c);
            match bdp {
                ND => {
                    // A dark non-digit acts as pad and terminates the decode.
                    nc_in = 0;
                    nti_eff = nac;
                    bdp = 0;
                    nbo -= 1;
                }
                WS => {
                    // Whitespace acts as pad and ends this group.
                    nti_eff = nac;
                    bdp = 0;
                    nbo -= 1;
                }
                PC => {
                    bdp = 0;
                    nbo -= 1;
                }
                _ => {}
            }
            qv = (qv << 6) | bdp as u32;
        }
        match nbo {
            3 => {
                out.push((qv >> 16) as u8);
                out.push((qv >> 8) as u8);
                out.push(qv as u8);
            }
            2 => {
                out.push((qv >> 16) as u8);
                out.push((qv >> 8) as u8);
            }
            1 => {
                out.push((qv >> 16) as u8);
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_matches_sqlite() {
        assert_eq!(encode(b"Hi"), "SGk=\n");
        assert_eq!(encode(&[0x00, 0x01, 0x02, 0x03, 0xff]), "AAECA/8=\n");
        assert_eq!(encode(b""), "");
    }

    #[test]
    fn round_trip() {
        for msg in [&b"Hello"[..], b"", b"a", b"ab", b"abc", &[0u8; 60]] {
            let enc = encode(msg);
            assert_eq!(decode(enc.as_bytes()), msg);
        }
    }

    #[test]
    fn decode_skips_whitespace_between_groups() {
        // Whitespace between 4-char groups (as in wrapped output) is skipped, so
        // a multi-line encoding round-trips…
        let long = [0u8; 60];
        assert_eq!(decode(encode(&long).as_bytes()), long);
        // …but whitespace *within* a group terminates it (matches sqlite:
        // `base64(char(83,71,10,32,107,61))` = X'4860', not "Hi").
        assert_eq!(decode(b"SG\n k="), [0x48, 0x60]);
    }
}
