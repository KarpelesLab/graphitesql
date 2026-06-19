//! SQLite "varint" — variable-length big-endian integer encoding.
//!
//! This is the encoding SQLite uses pervasively in the file format (record
//! headers, cell sizes, rowids, the freelist, the pointer map, …). It is
//! defined in the file-format spec, section "Variable-length integers":
//!
//! * A varint is between 1 and 9 bytes.
//! * For the first 8 bytes, the high bit signals "another byte follows" and the
//!   low 7 bits carry data, most-significant group first (big-endian).
//! * If a 9th byte is needed, it contributes all 8 of its bits (so a full 9-byte
//!   varint encodes a complete 64-bit value: 8*7 + 8 = 64 bits).
//!
//! The encoded value is logically a `u64`; SQLite reinterprets it as `i64`
//! where a signed value is wanted (two's complement), which is why
//! [`decode_i64`] simply transmutes the bits with `as`.

/// The maximum number of bytes a varint may occupy.
pub const MAX_LEN: usize = 9;

/// Decode a varint from the start of `buf`, returning `(value, bytes_read)`.
///
/// Returns `None` if `buf` does not contain a complete varint (i.e. it ends
/// mid-varint without a terminating byte and without reaching 9 bytes).
pub fn decode(buf: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut i = 0;
    while i < 8 {
        match buf.get(i) {
            None => return None,
            Some(&byte) => {
                result = (result << 7) | u64::from(byte & 0x7f);
                i += 1;
                if byte & 0x80 == 0 {
                    return Some((result, i));
                }
            }
        }
    }
    // Ninth byte: all 8 bits are data. We already accumulated 56 bits (8*7).
    match buf.get(8) {
        None => None,
        Some(&byte) => {
            result = (result << 8) | u64::from(byte);
            Some((result, 9))
        }
    }
}

/// Decode a varint and reinterpret it as a signed 64-bit integer.
pub fn decode_i64(buf: &[u8]) -> Option<(i64, usize)> {
    decode(buf).map(|(v, n)| (v as i64, n))
}

/// The number of bytes [`encode`] would use for `value`.
pub fn len(value: u64) -> usize {
    // Bytes 1..=8 hold 7 bits each; the 9th byte is only reached for values
    // that need more than 56 bits.
    match value {
        0..=0x7f => 1,
        0x80..=0x3fff => 2,
        0x4000..=0x1f_ffff => 3,
        0x20_0000..=0x0fff_ffff => 4,
        0x1000_0000..=0x0007_ffff_ffff => 5,
        0x8_0000_0000..=0x3ff_ffff_ffff => 6,
        0x400_0000_0000..=0x1_ffff_ffff_ffff => 7,
        0x2_0000_0000_0000..=0xff_ffff_ffff_ffff => 8,
        _ => 9,
    }
}

/// Encode `value` into `out`, returning the number of bytes written.
///
/// `out` must be at least [`MAX_LEN`] bytes long; this never writes more than
/// that. Panics only via the slice bounds check if `out` is too short, which is
/// a caller bug — pass a `[0u8; varint::MAX_LEN]` buffer.
pub fn encode(value: u64, out: &mut [u8]) -> usize {
    let n = len(value);
    if n == 9 {
        // Low 8 bits go in the last byte verbatim; the upper 56 bits are split
        // into seven 7-bit groups, each with its continuation bit set.
        out[8] = value as u8;
        let mut v = value >> 8;
        for slot in (0..8).rev() {
            out[slot] = (v as u8 & 0x7f) | 0x80;
            v >>= 7;
        }
        return 9;
    }
    // n <= 8: emit n groups of 7 bits, big-endian, continuation bit on all but
    // the last.
    let mut v = value;
    for idx in (0..n).rev() {
        let cont = if idx == n - 1 { 0 } else { 0x80 };
        out[idx] = (v as u8 & 0x7f) | cont;
        v >>= 7;
    }
    n
}

/// Encode `value` (signed) into `out`, returning the number of bytes written.
pub fn encode_i64(value: i64, out: &mut [u8]) -> usize {
    encode(value as u64, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encoding then decoding any value must round-trip, and the reported
    /// length must agree across `len`, `encode`, and `decode`.
    fn round_trip(v: u64) {
        let mut buf = [0u8; MAX_LEN];
        let written = encode(v, &mut buf);
        assert_eq!(written, len(v), "len disagrees with encode for {v:#x}");
        let (decoded, read) = decode(&buf).expect("decode complete varint");
        assert_eq!(decoded, v, "round-trip value mismatch for {v:#x}");
        assert_eq!(read, written, "read len != written len for {v:#x}");
    }

    #[test]
    fn round_trips_boundaries() {
        let boundaries = [
            0u64,
            1,
            0x7f,
            0x80,
            0x3fff,
            0x4000,
            0x1f_ffff,
            0x20_0000,
            0x0fff_ffff,
            0x1000_0000,
            0x7_ffff_ffff,
            0x8_0000_0000,
            0x3ff_ffff_ffff,
            0x400_0000_0000,
            0x1_ffff_ffff_ffff,
            0x2_0000_0000_0000,
            0xff_ffff_ffff_ffff,
            0x100_0000_0000_0000,
            u64::MAX,
        ];
        for v in boundaries {
            round_trip(v);
        }
    }

    #[test]
    fn lengths_are_correct() {
        assert_eq!(len(0), 1);
        assert_eq!(len(0x7f), 1);
        assert_eq!(len(0x80), 2);
        assert_eq!(len(0xff_ffff_ffff_ffff), 8);
        assert_eq!(len(0x100_0000_0000_0000), 9);
        assert_eq!(len(u64::MAX), 9);
    }

    #[test]
    fn known_encodings() {
        // Single byte.
        let mut b = [0u8; MAX_LEN];
        assert_eq!(encode(0, &mut b), 1);
        assert_eq!(b[0], 0x00);
        assert_eq!(encode(0x7f, &mut b), 1);
        assert_eq!(b[0], 0x7f);
        // 0x80 -> 0x81 0x00 (two 7-bit groups: 0b1, 0b0000000).
        assert_eq!(encode(0x80, &mut b), 2);
        assert_eq!(&b[..2], &[0x81, 0x00]);
        // 300 = 0x12c -> 0x82 0x2c.
        assert_eq!(encode(300, &mut b), 2);
        assert_eq!(&b[..2], &[0x82, 0x2c]);
        // u64::MAX -> nine 0xff bytes.
        assert_eq!(encode(u64::MAX, &mut b), 9);
        assert_eq!(b, [0xff; 9]);
    }

    #[test]
    fn decode_rejects_truncated() {
        // High bit set but no following byte.
        assert_eq!(decode(&[0x81]), None);
        assert_eq!(decode(&[]), None);
        // Eight continuation bytes but the buffer ends before the 9th.
        assert_eq!(decode(&[0x80; 8]), None);
    }

    #[test]
    fn decode_stops_at_first_terminator() {
        // Trailing bytes after a complete 1-byte varint are ignored.
        let (v, n) = decode(&[0x01, 0xff, 0xff]).unwrap();
        assert_eq!((v, n), (1, 1));
    }

    #[test]
    fn signed_round_trip() {
        for v in [0i64, -1, 1, i64::MIN, i64::MAX, -1000, 1000] {
            let mut b = [0u8; MAX_LEN];
            let n = encode_i64(v, &mut b);
            let (d, m) = decode_i64(&b).unwrap();
            assert_eq!((d, m), (v, n));
        }
    }
}
