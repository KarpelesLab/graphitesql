//! SHA-1 and SHA-3 (Keccak, FIPS-202) message digests.
//!
//! Faithful ports of SQLite's loadable extensions `ext/misc/sha1.c` and
//! `ext/misc/shathree.c`, backing the `sha1()` and `sha3()` SQL functions. Pure
//! `core`/`alloc`, no `unsafe`, no dependencies. The output is byte-for-byte the
//! same as the corresponding sqlite3 extension.

use alloc::vec::Vec;

/// Compute the SHA-1 digest of `data`, returned as its 20 raw bytes.
///
/// This is the hash `sha1(X)` renders (sqlite's extension returns the lower-case
/// hex of these bytes; the SQL layer keeps the raw blob).
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    // Message length in bits, before padding (matches the 64-bit big-endian
    // length appended by the SHA-1 padding scheme).
    let ml = (data.len() as u64).wrapping_mul(8);

    // Append 0x80, then 0x00 up to a 56-mod-64 boundary, then the 8-byte length.
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());

    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let b = &block[i * 4..i * 4 + 4];
            *word = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Round constants for the Keccak-f[1600] iota step.
const KECCAK_RC: [u64; 24] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808a,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808b,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008a,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000a,
    0x0000_0000_8000_808b,
    0x8000_0000_0000_008b,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800a,
    0x8000_0000_8000_000a,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];

/// Per-lane rotation offsets for the rho step, indexed `[x][y]` where the lane
/// index is `x + 5*y` (the standard Keccak rho table).
const KECCAK_ROT: [[u32; 5]; 5] = [
    [0, 36, 3, 41, 18],
    [1, 44, 10, 45, 2],
    [62, 6, 43, 15, 61],
    [28, 55, 25, 21, 56],
    [27, 20, 39, 8, 14],
];

/// One application of the Keccak-f[1600] permutation to the 25-lane state.
fn keccak_f1600(s: &mut [u64; 25]) {
    for &rc in KECCAK_RC.iter() {
        // theta
        let mut c = [0u64; 5];
        for x in 0..5 {
            c[x] = s[x] ^ s[x + 5] ^ s[x + 10] ^ s[x + 15] ^ s[x + 20];
        }
        let mut d = [0u64; 5];
        for x in 0..5 {
            d[x] = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
        }
        for x in 0..5 {
            for y in 0..5 {
                s[x + 5 * y] ^= d[x];
            }
        }

        // rho + pi
        let mut b = [0u64; 25];
        for x in 0..5 {
            for y in 0..5 {
                b[y + 5 * ((2 * x + 3 * y) % 5)] = s[x + 5 * y].rotate_left(KECCAK_ROT[x][y]);
            }
        }

        // chi
        for x in 0..5 {
            for y in 0..5 {
                s[x + 5 * y] = b[x + 5 * y] ^ ((!b[(x + 1) % 5 + 5 * y]) & b[(x + 2) % 5 + 5 * y]);
            }
        }

        // iota
        s[0] ^= rc;
    }
}

/// The sponge rate (bytes absorbed per permutation) for each SHA-3 variant.
fn sha3_rate(bits: u16) -> usize {
    match bits {
        224 => 144,
        256 => 136,
        384 => 104,
        512 => 72,
        // Callers validate `bits`; fall back to the SHA3-256 default.
        _ => 136,
    }
}

/// Compute the SHA-3 (FIPS-202) digest of `data` at the given size in bits
/// (`224`, `256`, `384`, or `512`), returned as `bits/8` raw bytes.
pub fn sha3(data: &[u8], bits: u16) -> Vec<u8> {
    let rate = sha3_rate(bits);
    let out_len = (bits / 8) as usize;

    let mut state = [0u64; 25];
    let mut block = [0u8; 144]; // max rate (SHA3-224)
    let mut idx = 0usize;

    // Absorb full rate-sized blocks.
    for &byte in data {
        block[idx] = byte;
        idx += 1;
        if idx == rate {
            absorb(&mut state, &block[..rate]);
            keccak_f1600(&mut state);
            idx = 0;
        }
    }

    // FIPS-202 SHA-3 padding: append 0x06, pad with zeros, set the high bit of
    // the final rate byte (0x80). When only one byte of room remains the two
    // markers land in the same byte (0x86).
    for byte in block.iter_mut().take(rate).skip(idx) {
        *byte = 0;
    }
    block[idx] ^= 0x06;
    block[rate - 1] ^= 0x80;
    absorb(&mut state, &block[..rate]);
    keccak_f1600(&mut state);

    // Squeeze: the digest is short enough to come from the first output block.
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        out.push((state[i / 8] >> (8 * (i % 8))) as u8);
    }
    out
}

/// XOR a rate-sized block (a multiple of 8 bytes) into the state as
/// little-endian 64-bit lanes.
fn absorb(state: &mut [u64; 25], block: &[u8]) {
    for (i, chunk) in block.as_chunks::<8>().0.iter().enumerate() {
        state[i] ^= u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn hex(bytes: &[u8]) -> alloc::string::String {
        let mut s = alloc::string::String::new();
        for b in bytes {
            s.push_str(&alloc::format!("{b:02x}"));
        }
        s
    }

    #[test]
    fn sha1_known_vectors() {
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn sha3_256_known_vectors() {
        assert_eq!(
            hex(&sha3(b"", 256)),
            "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a"
        );
        assert_eq!(
            hex(&sha3(b"abc", 256)),
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
        );
    }

    #[test]
    fn sha3_other_sizes() {
        // FIPS-202 known-answer digests for the empty message.
        assert_eq!(
            hex(&sha3(b"", 224)),
            "6b4e03423667dbb73b6e15454f0eb1abd4597f9a1b078e3f5b5a6bc7"
        );
        assert_eq!(
            hex(&sha3(b"", 384)),
            "0c63a75b845e4f7d01107d852e4c2485c51a50aaaa94fc61995e71bbee983a2ac3713831264adb47fb6bd1e058d5f004"
        );
        assert_eq!(
            hex(&sha3(b"", 512)),
            "a69f73cca23a9ac5c8b567dc185a756e97c982164fe25859e0d1dcc1475c80a615b2123af1f5f94c11e3e9402c3ac558f500199d95b6d3e301758586281dcd26"
        );
        // Length checks.
        assert_eq!(sha3(b"abc", 224).len(), 28);
        assert_eq!(sha3(b"abc", 512).len(), 64);
        let _: Vec<u8> = sha3(b"abc", 384);
    }
}
