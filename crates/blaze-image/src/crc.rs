//! CRC-32C (Castagnoli) of page groups in the CPU's CRC instructions: SSE4.2 `crc32q` on x86-64,
//! ARMv8 `crc32cx` on aarch64, each run as three independent streams so the instruction's
//! three-cycle latency is hidden (about 8 bytes a cycle, the speed of Intel's `crc_pcl` without
//! its carry-less folding), and slicing-by-8 tables where the CPU has neither. Every path
//! computes the same function, checked against each other in the tests: which one runs is
//! dispatch, not a fallback.

/// The Castagnoli polynomial, reflected.
const POLY: u32 = 0x82F6_3B78;

/// `TABLES[0]` is the byte-at-a-time table; `TABLES[k][b]` is byte `b` followed by `k` zero bytes.
const TABLES: [[u32; 256]; 8] = tables();

const fn tables() -> [[u32; 256]; 8] {
    let mut out = [[0; 256]; 8];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ POLY } else { crc >> 1 };
            bit += 1;
        }
        out[0][byte] = crc;
        byte += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut byte = 0;
        while byte < 256 {
            let previous = out[k - 1][byte];
            out[k][byte] = (previous >> 8) ^ out[0][(previous & 0xff) as usize];
            byte += 1;
        }
        k += 1;
    }
    out
}

/// CRC-32C (reflected polynomial 0x82F63B78, initial !0, final xor !0) of `bytes`.
pub fn crc32c(bytes: &[u8]) -> u32 {
    extend(0, bytes)
}

/// The CRC-32C of `a` followed by `bytes`, given `crc == crc32c(a)`.
pub(crate) fn extend(crc: u32, bytes: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.2") {
        // SAFETY: the CPU has SSE4.2, checked just above.
        return !unsafe { x86::raw(!crc, bytes) };
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("crc") {
        // SAFETY: the CPU has the CRC instructions, checked just above.
        return !unsafe { arm::raw(!crc, bytes) };
    }
    !portable(!crc, bytes)
}

fn word(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("8 bytes"))
}

/// The register (no initial or final inversion) after `bytes`, starting from `state`.
fn portable(mut state: u32, bytes: &[u8]) -> u32 {
    let mut words = bytes.chunks_exact(8);
    for bytes in &mut words {
        let v = word(bytes) ^ u64::from(state);
        let at = |k: u32| ((v >> (8 * k)) & 0xff) as usize;
        state = TABLES[7][at(0)]
            ^ TABLES[6][at(1)]
            ^ TABLES[5][at(2)]
            ^ TABLES[4][at(3)]
            ^ TABLES[3][at(4)]
            ^ TABLES[2][at(5)]
            ^ TABLES[1][at(6)]
            ^ TABLES[0][at(7)];
    }
    for &byte in words.remainder() {
        state = (state >> 8) ^ TABLES[0][((state ^ u32::from(byte)) & 0xff) as usize];
    }
    state
}

/// Bytes per stream in one round of the three-stream loops: a power of two, so [`SHIFT`] is
/// built by squaring. A 64 KiB group is five rounds and a 4 KiB tail.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const STREAM: usize = 4096;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const _: () = assert!(STREAM.is_power_of_two());

/// What [`STREAM`] zero bytes do to a register: a linear map over GF(2) (multiplication by
/// x^(8·STREAM) mod the polynomial), split by input byte, so applying it is four lookups.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SHIFT: [[u32; 256]; 4] = shift_table();

/// The image of `v` under the linear map whose image of bit `i` is `columns[i]`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const fn apply(columns: &[u32; 32], v: u32) -> u32 {
    let mut out = 0;
    let mut bit = 0;
    while bit < 32 {
        if (v >> bit) & 1 == 1 {
            out ^= columns[bit];
        }
        bit += 1;
    }
    out
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const fn shift_table() -> [[u32; 256]; 4] {
    let mut columns = [0; 32];
    let mut bit = 0;
    while bit < 32 {
        let state = 1u32 << bit;
        columns[bit] = (state >> 8) ^ TABLES[0][(state & 0xff) as usize];
        bit += 1;
    }
    let mut bytes = 1;
    while bytes < STREAM {
        let mut squared = [0; 32];
        let mut bit = 0;
        while bit < 32 {
            squared[bit] = apply(&columns, columns[bit]);
            bit += 1;
        }
        columns = squared;
        bytes *= 2;
    }
    let mut out = [[0; 256]; 4];
    let mut k = 0;
    while k < 4 {
        let mut byte = 0;
        while byte < 256 {
            out[k][byte] = apply(&columns, (byte as u32) << (8 * k));
            byte += 1;
        }
        k += 1;
    }
    out
}

/// The register after [`STREAM`] zero bytes, starting from `state`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn shift(state: u32) -> u32 {
    SHIFT[0][(state & 0xff) as usize]
        ^ SHIFT[1][((state >> 8) & 0xff) as usize]
        ^ SHIFT[2][((state >> 16) & 0xff) as usize]
        ^ SHIFT[3][(state >> 24) as usize]
}

/// The register after three consecutive [`STREAM`]-byte blocks, from the registers of each run
/// alone: `a` from the real starting state, `b` and `c` from 0. A register is linear in its
/// start and its bytes, so a block's run from 0 adds onto the shifted register before it.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn combine(a: u32, b: u32, c: u32) -> u32 {
    shift(shift(a) ^ b) ^ c
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};

    use super::{STREAM, combine, word};

    /// The register after `bytes`, starting from `state`.
    #[target_feature(enable = "sse4.2")]
    pub(super) fn raw(mut state: u32, mut bytes: &[u8]) -> u32 {
        while bytes.len() >= 3 * STREAM {
            let (a, rest) = bytes.split_at(STREAM);
            let (b, rest) = rest.split_at(STREAM);
            let (c, rest) = rest.split_at(STREAM);
            let (mut x, mut y, mut z) = (u64::from(state), 0, 0);
            for ((a, b), c) in a.chunks_exact(8).zip(b.chunks_exact(8)).zip(c.chunks_exact(8)) {
                x = _mm_crc32_u64(x, word(a));
                y = _mm_crc32_u64(y, word(b));
                z = _mm_crc32_u64(z, word(c));
            }
            state = combine(x as u32, y as u32, z as u32);
            bytes = rest;
        }
        let mut words = bytes.chunks_exact(8);
        let mut wide = u64::from(state);
        for bytes in &mut words {
            wide = _mm_crc32_u64(wide, word(bytes));
        }
        let mut state = wide as u32;
        for &byte in words.remainder() {
            state = _mm_crc32_u8(state, byte);
        }
        state
    }
}

#[cfg(target_arch = "aarch64")]
mod arm {
    use std::arch::aarch64::{__crc32cb, __crc32cd};

    use super::{STREAM, combine, word};

    /// The register after `bytes`, starting from `state`.
    #[target_feature(enable = "crc")]
    pub(super) fn raw(mut state: u32, mut bytes: &[u8]) -> u32 {
        while bytes.len() >= 3 * STREAM {
            let (a, rest) = bytes.split_at(STREAM);
            let (b, rest) = rest.split_at(STREAM);
            let (c, rest) = rest.split_at(STREAM);
            let (mut x, mut y, mut z) = (state, 0, 0);
            for ((a, b), c) in a.chunks_exact(8).zip(b.chunks_exact(8)).zip(c.chunks_exact(8)) {
                x = __crc32cd(x, word(a));
                y = __crc32cd(y, word(b));
                z = __crc32cd(z, word(c));
            }
            state = combine(x, y, z);
            bytes = rest;
        }
        let mut words = bytes.chunks_exact(8);
        for bytes in &mut words {
            state = __crc32cd(state, word(bytes));
        }
        for &byte in words.remainder() {
            state = __crc32cb(state, byte);
        }
        state
    }
}

/// The reference every path is checked against: the bitwise definition, one bit a step.
#[cfg(test)]
pub(crate) fn bitwise(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ POLY } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
fn extend_portable(crc: u32, bytes: &[u8]) -> u32 {
    !portable(!crc, bytes)
}

#[cfg(all(test, target_arch = "x86_64"))]
fn extend_sse42(crc: u32, bytes: &[u8]) -> u32 {
    assert!(std::arch::is_x86_feature_detected!("sse4.2"));
    // SAFETY: the CPU has SSE4.2, asserted just above.
    !unsafe { x86::raw(!crc, bytes) }
}

#[cfg(all(test, target_arch = "aarch64"))]
fn extend_crc(crc: u32, bytes: &[u8]) -> u32 {
    assert!(std::arch::is_aarch64_feature_detected!("crc"));
    // SAFETY: the CPU has the CRC instructions, asserted just above.
    !unsafe { arm::raw(!crc, bytes) }
}

/// Every path this CPU can run, the portable one first, for the differential tests.
#[cfg(test)]
pub(crate) fn paths() -> Vec<(&'static str, fn(u32, &[u8]) -> u32)> {
    let mut out: Vec<(&'static str, fn(u32, &[u8]) -> u32)> = vec![("portable", extend_portable)];
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.2") {
        out.push(("sse4.2", extend_sse42));
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("crc") {
        out.push(("crc", extend_crc));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Rng;

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut rng = Rng(seed);
        (0..len).map(|_| rng.next() as u8).collect()
    }

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(&[0; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xff; 32]), 0x62A8_AB43);
        for (name, path) in paths() {
            assert_eq!(path(0, b"123456789"), 0xE306_9283, "{name}");
            assert_eq!(path(0, b""), 0, "{name}");
            assert_eq!(path(0, &[0; 32]), 0x8A91_36AA, "{name}");
        }
    }

    #[test]
    fn every_path_is_the_bitwise_definition() {
        let buffer = noise(300 + 16, 1);
        for offset in 0..16 {
            for len in 0..=300 {
                let bytes = &buffer[offset..offset + len];
                let want = bitwise(bytes);
                assert_eq!(crc32c(bytes), want, "{offset}+{len}");
                for (name, path) in paths() {
                    assert_eq!(path(0, bytes), want, "{name}: {offset}+{len}");
                }
            }
        }
    }

    /// Lengths around the three-stream rounds and whole groups of every size the image uses.
    #[test]
    fn every_path_agrees_on_groups() {
        let lens = [4096, 12_287, 12_288, 12_289, 16_384, 65_536, 65_536 + 3, 1 << 20];
        let buffer = noise((1 << 20) + 16, 3);
        let mut rng = Rng(4);
        for offset in [0, 1, 7, 8, 15] {
            for len in lens {
                let bytes = &buffer[offset..offset + len];
                let start = rng.next() as u32;
                let want = extend_portable(start, bytes);
                for (name, path) in paths() {
                    assert_eq!(path(0, bytes), extend_portable(0, bytes), "{name}: {offset}+{len}");
                    assert_eq!(path(start, bytes), want, "{name}: {offset}+{len} from {start:#x}");
                }
            }
        }
        assert_eq!(crc32c(&buffer[..65_536]), bitwise(&buffer[..65_536]));
    }

    #[test]
    fn extend_continues_a_crc() {
        let bytes = noise(50_000, 5);
        let mut rng = Rng(6);
        let whole = crc32c(&bytes);
        for _ in 0..200 {
            let at = (rng.next() % (bytes.len() as u64 + 1)) as usize;
            let (a, b) = bytes.split_at(at);
            assert_eq!(extend(crc32c(a), b), whole, "split at {at}");
            for (name, path) in paths() {
                assert_eq!(path(path(0, a), b), whole, "{name}: split at {at}");
            }
        }
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn shift_is_stream_zero_bytes() {
        let zeros = vec![0u8; STREAM];
        let mut rng = Rng(7);
        for state in [0, 1, !0, 0x8000_0000].into_iter().chain((0..100).map(|_| rng.next() as u32)) {
            assert_eq!(shift(state), portable(state, &zeros), "{state:#x}");
        }
    }

    /// Any one flipped bit of a group changes its CRC: what makes every corruption of one bit
    /// a [`crate::Corrupt`].
    #[test]
    fn one_flipped_bit_changes_the_crc() {
        let mut bytes = noise(4096, 8);
        let whole = crc32c(&bytes);
        for bit in (0..4096 * 8).step_by(13) {
            bytes[bit / 8] ^= 1 << (bit % 8);
            assert_ne!(crc32c(&bytes), whole, "bit {bit}");
            bytes[bit / 8] ^= 1 << (bit % 8);
        }
    }
}
