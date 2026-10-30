//! StreamVByte (Lemire, Kurz and Rupp, "Stream VByte: Faster Byte-Oriented Integer Compression",
//! 2017) for a memo's edges at rest, as zigzag deltas of their 31-bit nodes.
//!
//! A stream of `n` values is `(n + 3) / 4` control bytes, two bits a value (its byte length minus
//! one), then each value's 1 to 4 little-endian bytes. The control bytes come first so a group of
//! four decodes with one 16-byte load, one byte shuffle chosen by its control byte and one store,
//! and a list is skipped from its control bytes alone.
//!
//! The bytes come from a mapped image; every read is bounds-checked first and a stream that ends
//! early is [`Truncated`], never a panic.

use std::simd::{simd_swizzle, u8x16, u32x4};

/// A stream ended before the values it promised.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Truncated;

/// The node bits of an edge word; bit 31 is the output flag.
const NODE: u32 = 0x7fff_ffff;

/// The most bytes a LEB128 `usize` count takes.
const MAX_COUNT_LEN: usize = 10;

/// The most bytes `encode` writes for `n` values: `(n + 3) / 4` control bytes and 4 per value.
pub const fn max_len(n: usize) -> usize {
    n.div_ceil(4) + 4 * n
}

/// The byte length of value `lane` of a group with control byte `control`.
const fn field_len(control: usize, lane: usize) -> usize {
    ((control >> (2 * lane)) & 3) + 1
}

/// For each control byte, the byte shuffle that spreads a group's packed bytes over four
/// little-endian `u32` lanes; 0xff (out of range) zeroes a lane's unused high bytes.
const fn shuffle_table() -> [[u8; 16]; 256] {
    let mut table = [[0xff; 16]; 256];
    let mut control = 0;
    while control < 256 {
        let mut source = 0u8;
        let mut lane = 0;
        while lane < 4 {
            let len = field_len(control, lane);
            let mut byte = 0;
            while byte < len {
                table[control][4 * lane + byte] = source;
                source += 1;
                byte += 1;
            }
            lane += 1;
        }
        control += 1;
    }
    table
}

/// For each control byte, the data bytes of its group of four.
const fn group_len_table() -> [u8; 256] {
    let mut table = [0; 256];
    let mut control = 0;
    while control < 256 {
        let len = field_len(control, 0) + field_len(control, 1) + field_len(control, 2) + field_len(control, 3);
        table[control] = len as u8;
        control += 1;
    }
    table
}

static SHUFFLE: [[u8; 16]; 256] = shuffle_table();
static GROUP_LEN: [u8; 256] = group_len_table();

/// A value's byte length minus one: 0 for 0 to 255, up to 3 at 2^24 and above.
#[inline]
fn byte_code(value: u32) -> usize {
    ((31 - (value | 1).leading_zeros()) / 8) as usize
}

/// Appends the StreamVByte encoding of `values` to `out`: first the `(n + 3) / 4` control bytes
/// (value `i`'s byte length minus one in bits `2 * (i % 4)` of control byte `i / 4`; unused
/// trailing fields 0), then each value's 1 to 4 little-endian bytes in order. Returns the bytes
/// appended. Reserves once (`max_len`), never per value.
pub fn encode(values: &[u32], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    let control_len = values.len().div_ceil(4);
    out.resize(start + max_len(values.len()), 0);
    let (control, data) = out[start..].split_at_mut(control_len);
    let mut at = 0;
    for (group, control) in values.chunks(4).zip(control) {
        let mut bits = 0;
        for (lane, &value) in group.iter().enumerate() {
            let code = byte_code(value);
            bits |= (code as u8) << (2 * lane);
            // Writes all four bytes and keeps `code + 1`: the next value overwrites the rest,
            // and `at + 4 <= 4 * (i + 1) <= data.len()` for value `i`.
            data[at..at + 4].copy_from_slice(&value.to_le_bytes());
            at += code + 1;
        }
        *control = bits;
    }
    out.truncate(start + control_len + at);
    control_len + at
}

/// Decodes `out.len()` values from the front of `bytes`, returning the bytes read. Groups of four
/// with 16 readable bytes after their data start go through the SIMD path (one 16-byte load, one
/// `swizzle_dyn` by a 256-entry table of shuffle masks indexed by the control byte, one 16-byte
/// store); the rest through the scalar path. Bit-identical to `decode_scalar`.
///
/// A last group of one to three values with 16 readable bytes takes the SIMD path as well, so a
/// short list in the middle of a stream decodes with no scalar step.
pub fn decode(bytes: &[u8], out: &mut [u32]) -> Result<usize, Truncated> {
    let control_len = out.len().div_ceil(4);
    let (control, _) = bytes.split_at_checked(control_len).ok_or(Truncated)?;
    let mut at = control_len;
    let mut done = 0;
    let (groups, _) = out.as_chunks_mut::<4>();
    for (group, &code) in groups.iter_mut().zip(control) {
        // `at <= bytes.len()` throughout: each step adds at most 16 to an `at` with 16 bytes after
        // it. Once one group lacks its 16 bytes every later one does, so the rest go scalar.
        let Some(window) = bytes.get(at..at + 16) else { break };
        let shuffled = u8x16::from_slice(window).swizzle_dyn(u8x16::from_array(SHUFFLE[code as usize]));
        let shuffled = shuffled.to_array();
        for (word, le) in group.iter_mut().zip(shuffled.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*le);
        }
        at += GROUP_LEN[code as usize] as usize;
        done += 4;
    }
    // A last partial group takes the shuffle too: its used lanes' positions depend only on the
    // fields before them, and unused fields (masked to 0) each count one byte in `GROUP_LEN`.
    let used = out.len() % 4;
    if done + used == out.len()
        && used != 0
        && let Some(window) = bytes.get(at..at + 16)
    {
        let code = control[done / 4];
        let shuffled = u8x16::from_slice(window).swizzle_dyn(u8x16::from_array(SHUFFLE[code as usize]));
        let shuffled = shuffled.to_array();
        for (word, le) in out[done..].iter_mut().zip(shuffled.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*le);
        }
        let mask = (1u8 << (2 * used)) - 1;
        return Ok(at + GROUP_LEN[(code & mask) as usize] as usize - (4 - used));
    }
    decode_from(bytes, control, at, done, out)
}

/// The reference decoder: one value at a time.
pub fn decode_scalar(bytes: &[u8], out: &mut [u32]) -> Result<usize, Truncated> {
    let control_len = out.len().div_ceil(4);
    let (control, _) = bytes.split_at_checked(control_len).ok_or(Truncated)?;
    decode_from(bytes, control, control_len, 0, out)
}

/// Decodes values `from..` of `out` one at a time, value `from`'s data starting at `bytes[at]`.
fn decode_from(bytes: &[u8], control: &[u8], mut at: usize, from: usize, out: &mut [u32]) -> Result<usize, Truncated> {
    for (index, word) in (from..).zip(&mut out[from..]) {
        let len = field_len(control[index / 4] as usize, index % 4);
        let field = bytes.get(at..at + len).ok_or(Truncated)?;
        let mut le = [0; 4];
        le[..len].copy_from_slice(field);
        *word = u32::from_le_bytes(le);
        at += len;
    }
    Ok(at)
}

/// The bytes the encoding of `n` values at the front of `bytes` takes, from its control bytes
/// alone (a 256-entry table of group lengths), to skip a list without decoding it.
///
/// Eight full groups at a time are summed by popcount (a group's data is 4 plus its four 2-bit
/// fields), the rest by the table; a last partial group counts only its used fields, as `decode`
/// reads them.
pub fn encoded_len(bytes: &[u8], n: usize) -> Result<usize, Truncated> {
    const LOW: u64 = 0x5555_5555_5555_5555;
    let control_len = n.div_ceil(4);
    let control = bytes.get(..control_len).ok_or(Truncated)?;
    let (full, partial) = control.split_at(n / 4);
    let (words, rest) = full.as_chunks::<8>();
    let mut fields = 0;
    for word in words {
        let word = u64::from_le_bytes(*word);
        fields += (word & LOW).count_ones() as usize + 2 * (word & (LOW << 1)).count_ones() as usize;
    }
    for &code in rest {
        fields += GROUP_LEN[code as usize] as usize - 4;
    }
    if let Some(&code) = partial.first() {
        let used = (code as u32 & ((1 << (2 * (n % 4))) - 1)) as u64;
        fields += (used & LOW).count_ones() as usize + 2 * (used & (LOW << 1)).count_ones() as usize;
    }
    // `control_len <= bytes.len()`, so `n <= 4 * bytes.len() + 3` and this cannot overflow.
    let len = control_len + n + fields;
    if len > bytes.len() { Err(Truncated) } else { Ok(len) }
}

/// Zigzag of the 31-bit signed number in the low 31 bits of `delta`: 0, -1, 1, -2, ... to 0, 1, 2,
/// 3, ..., below 2^31.
#[inline]
fn zigzag31(delta: u32) -> u32 {
    let signed = ((delta << 1) as i32) >> 1;
    (((signed << 1) ^ (signed >> 30)) as u32) & NODE
}

/// The inverse of `zigzag31`, as a two's-complement `u32` to add mod 2^31.
#[inline]
fn unzigzag31(zigzag: u32) -> u32 {
    (zigzag >> 1) ^ (zigzag & 1).wrapping_neg()
}

/// Edge words to the values `encode` packs. A word is a node (low 31 bits) and an output flag
/// (bit 31). Value `i` is `zigzag31(node_i - node_{i-1}) << 1 | output_i`, the difference taken
/// mod 2^31 and read as a signed 31-bit number (so it fits: zigzag of a 31-bit signed number is
/// below 2^31), with `node_{-1} = base` (the memo's own node: in DFS preorder its first
/// dependencies follow it, so deltas are small and positive). `out.len() == words.len()`.
pub fn delta_encode(words: &[u32], base: u32, out: &mut [u32]) {
    assert_eq!(words.len(), out.len(), "delta_encode: one value per word");
    let mut previous = base & NODE;
    for (value, &word) in out.iter_mut().zip(words) {
        let node = word & NODE;
        *value = zigzag31(node.wrapping_sub(previous)) << 1 | word >> 31;
        previous = node;
    }
}

/// The inverse of `delta_encode`, in place: values back to edge words.
///
/// Four at a time: the deltas' prefix sum within a group by two lane shifts and adds, then the
/// running node added to all four, so the loop-carried chain is one scalar add a group.
pub fn delta_decode(values: &mut [u32], base: u32) {
    let zero = u32x4::splat(0);
    let one = u32x4::splat(1);
    // Nodes accumulate mod 2^32 and are masked on the way out: 2^31 divides 2^32.
    let mut node = base;
    let (groups, tail) = values.as_chunks_mut::<4>();
    for group in groups {
        let values = u32x4::from_array(*group);
        let zigzag = values >> one;
        let deltas = (zigzag >> one) ^ (zero - (zigzag & one));
        let sums = deltas + simd_swizzle!(deltas, zero, [4, 0, 1, 2]);
        let sums = sums + simd_swizzle!(sums, zero, [4, 4, 0, 1]);
        let nodes = sums + u32x4::splat(node);
        node = node.wrapping_add(sums[3]);
        *group = ((nodes & u32x4::splat(NODE)) | (values << u32x4::splat(31))).to_array();
    }
    for value in tail {
        node = node.wrapping_add(unzigzag31(*value >> 1));
        *value = (node & NODE) | (*value << 31);
    }
}

/// Appends `count` as LEB128, returning its bytes.
fn write_count(mut count: usize, out: &mut Vec<u8>) -> usize {
    let mut written = 1;
    while count >= 0x80 {
        out.push(count as u8 | 0x80);
        count >>= 7;
        written += 1;
    }
    out.push(count as u8);
    written
}

/// A LEB128 count at the front of `bytes` and the bytes it took. More than `MAX_COUNT_LEN` bytes,
/// or more than a `usize` holds, can't be a count `write_count` wrote.
fn read_count(bytes: &[u8]) -> Result<(usize, usize), Truncated> {
    let mut count = 0u64;
    for (index, &byte) in bytes.iter().take(MAX_COUNT_LEN).enumerate() {
        count |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok((usize::try_from(count).map_err(|_| Truncated)?, index + 1));
        }
    }
    Err(Truncated)
}

/// `encode(delta_encode(words, base))` appended to `out`, with the count first as LEB128 (one
/// byte below 128). Returns the bytes appended. Uses a caller buffer for the deltas
/// (`scratch`, cleared and reused: no allocation once it is big enough).
pub fn encode_edges(words: &[u32], base: u32, scratch: &mut Vec<u32>, out: &mut Vec<u8>) -> usize {
    scratch.clear();
    scratch.resize(words.len(), 0);
    delta_encode(words, base, scratch);
    out.reserve(MAX_COUNT_LEN + max_len(words.len()));
    let head = write_count(words.len(), out);
    head + encode(scratch, out)
}

/// Reads a list `encode_edges` wrote at the front of `bytes`: its count, then its words into
/// `out` (cleared, then filled; no allocation once big enough). Returns the bytes read.
pub fn decode_edges(bytes: &[u8], base: u32, out: &mut Vec<u32>) -> Result<usize, Truncated> {
    out.clear();
    let (count, head) = read_count(bytes)?;
    let rest = &bytes[head..];
    // Every value takes at least one byte: a larger count is truncated, and must not size `out`.
    if count > rest.len() {
        return Err(Truncated);
    }
    out.resize(count, 0);
    let read = decode(rest, out)?;
    delta_decode(out, base);
    Ok(head + read)
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;

    const EDGES: [u32; 8] = [0, 0xff, 0x100, 0xffff, 0x1_0000, 0xff_ffff, 0x100_0000, u32::MAX];

    /// A value of a random byte width, or one at a width's edge.
    fn value(rng: &mut SplitMix64) -> u32 {
        match rng.below(5) {
            0 => rng.below(1 << 8) as u32,
            1 => rng.range(1 << 8..1 << 16) as u32,
            2 => rng.range(1 << 16..1 << 24) as u32,
            3 => rng.range(1 << 24..1 << 32) as u32,
            _ => *rng.pick(&EDGES).unwrap(),
        }
    }

    fn values(rng: &mut SplitMix64, len: usize) -> Vec<u32> {
        (0..len).map(|_| value(rng)).collect()
    }

    /// An edge word: mostly near the last node, sometimes far, sometimes at the 31-bit wrap.
    fn words(rng: &mut SplitMix64, base: u32, len: usize) -> Vec<u32> {
        let mut node = base & NODE;
        (0..len)
            .map(|_| {
                node = match rng.below(4) {
                    0 => rng.below(1 << 31) as u32,
                    1 => *rng.pick(&[0, 1, NODE - 1, NODE, 1 << 30, (1 << 30) - 1]).unwrap(),
                    _ => node.wrapping_add(rng.range(0..64) as u32).wrapping_sub(16) & NODE,
                };
                node | (rng.coin() as u32) << 31
            })
            .collect()
    }

    fn delta_decode_scalar(values: &mut [u32], base: u32) {
        let mut node = base & NODE;
        for value in values {
            let delta = unzigzag31(*value >> 1) & NODE;
            node = (node + delta) & NODE;
            *value = node | (*value & 1) << 31;
        }
    }

    #[test]
    fn exact_format() {
        let mut out = vec![0xee];
        assert_eq!(encode(&EDGES, &mut out), 22);
        assert_eq!(out.len(), 23, "appends after what was there");
        assert_eq!(&out[..3], &[0xee, 0b01_01_00_00, 0b11_11_10_10]);
        assert_eq!(
            &out[3..],
            &[0, 0xff, 0, 1, 0xff, 0xff, 0, 0, 1, 0xff, 0xff, 0xff, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff][..]
        );

        let mut out = Vec::new();
        assert_eq!(encode(&[0x1234, 7], &mut out), 4);
        assert_eq!(out, [0b00_01, 0x34, 0x12, 7], "unused trailing fields are 0");
        assert_eq!(encode(&[], &mut out), 0);
        assert_eq!(out.len(), 4);
        assert_eq!(max_len(0), 0);
        assert_eq!(max_len(5), 22);
    }

    #[test]
    fn tables() {
        assert_eq!(GROUP_LEN[0], 4);
        assert_eq!(GROUP_LEN[0xff], 16);
        assert_eq!(GROUP_LEN[0b11_00_01_10], 3 + 2 + 1 + 4);
        assert_eq!(SHUFFLE[0], [0, 0xff, 0xff, 0xff, 1, 0xff, 0xff, 0xff, 2, 0xff, 0xff, 0xff, 3, 0xff, 0xff, 0xff]);
        assert_eq!(SHUFFLE[0xff], std::array::from_fn(|i| i as u8));
    }

    #[test]
    fn round_trips() {
        let mut rng = SplitMix64::new(0x5eed);
        let mut bytes = Vec::new();
        for len in 0..300 {
            for round in 0..4 {
                let input = match round {
                    0 => values(&mut rng, len),
                    1 => (0..len).map(|_| rng.below(1 << 8) as u32).collect(),
                    2 => (0..len).map(|_| rng.next_u32()).collect(),
                    _ => (0..len).map(|i| EDGES[i % EDGES.len()]).collect(),
                };
                bytes.clear();
                let written = encode(&input, &mut bytes);
                assert_eq!(written, bytes.len());
                assert!(written <= max_len(len));
                assert_eq!(encoded_len(&bytes, len), Ok(written));

                // Exact: the scalar path takes the last groups. Padded: SIMD takes every group.
                for pad in [0, 3, 16] {
                    let mut padded = bytes.clone();
                    padded.extend(std::iter::repeat_n(0xab, pad));
                    let mut fast = vec![0; len];
                    let mut slow = vec![0xdead; len];
                    assert_eq!(decode(&padded, &mut fast), Ok(written), "len {len} pad {pad}");
                    assert_eq!(decode_scalar(&padded, &mut slow), Ok(written), "len {len} pad {pad}");
                    assert_eq!(fast, input);
                    assert_eq!(slow, input);
                    assert_eq!(encoded_len(&padded, len), Ok(written));
                }
            }
        }
    }

    #[test]
    fn every_truncation_is_an_error() {
        let mut rng = SplitMix64::new(7);
        let mut scratch = Vec::new();
        for len in [1, 2, 3, 4, 5, 7, 8, 9, 16, 17, 33, 64, 130] {
            let input = values(&mut rng, len);
            let mut bytes = Vec::new();
            let written = encode(&input, &mut bytes);
            let mut out = vec![0; len];
            for cut in 0..written {
                let prefix = &bytes[..cut];
                assert_eq!(decode(prefix, &mut out), Err(Truncated), "len {len} cut {cut}");
                assert_eq!(decode_scalar(prefix, &mut out), Err(Truncated), "len {len} cut {cut}");
                assert_eq!(encoded_len(prefix, len), Err(Truncated), "len {len} cut {cut}");
            }

            let words = words(&mut rng, 99, len);
            let mut edges = Vec::new();
            let written = encode_edges(&words, 99, &mut scratch, &mut edges);
            let mut decoded = Vec::new();
            for cut in 0..written {
                assert_eq!(decode_edges(&edges[..cut], 99, &mut decoded), Err(Truncated), "len {len} cut {cut}");
            }
        }
        let mut decoded = Vec::new();
        assert_eq!(decode_edges(&[0xff; 11], 0, &mut decoded), Err(Truncated), "an over-long count");
        assert_eq!(decode_edges(&[0x80, 0x80, 0x80, 0x01, 0], 0, &mut decoded), Err(Truncated), "a count past the bytes");
    }

    #[test]
    fn garbage_never_panics_and_paths_agree() {
        let mut rng = SplitMix64::new(11);
        let mut fast = Vec::new();
        let mut slow = Vec::new();
        let mut words = Vec::new();
        for _ in 0..2000 {
            let mut bytes = vec![0; rng.index(80)];
            rng.fill(&mut bytes);
            let n = rng.index(70);
            fast.clear();
            fast.resize(n, 0);
            slow.clear();
            slow.resize(n, 0);
            let read = decode(&bytes, &mut fast);
            assert_eq!(read, decode_scalar(&bytes, &mut slow));
            assert_eq!(read, encoded_len(&bytes, n));
            if read.is_ok() {
                assert_eq!(fast, slow);
            }
            let _ = decode_edges(&bytes, rng.next_u32(), &mut words);
        }
    }

    #[test]
    fn zigzag_edges() {
        let wrap = |d: i32| (d as u32) & NODE;
        assert_eq!(zigzag31(0), 0);
        assert_eq!(zigzag31(wrap(-1)), 1);
        assert_eq!(zigzag31(1), 2);
        assert_eq!(zigzag31(wrap(-2)), 3);
        assert_eq!(zigzag31(2), 4);
        assert_eq!(zigzag31((1 << 30) - 1), (1 << 31) - 2);
        assert_eq!(zigzag31(wrap(-(1 << 30) + 1)), (1 << 31) - 3);
        assert_eq!(zigzag31(wrap(-(1 << 30))), (1 << 31) - 1);
        assert_eq!(zigzag31(1 << 30), (1 << 31) - 1, "2^30 is -2^30 mod 2^31");
        for delta in [0, 1, 2, (1 << 30) - 1, 1 << 30, NODE, NODE - 1, 12345, NODE - 12345] {
            assert_eq!(unzigzag31(zigzag31(delta)) & NODE, delta);
        }

        // From node 2^31 - 1 to 0 is +1, and back is -1.
        let mut out = [0; 3];
        delta_encode(&[NODE, 0, NODE | 1 << 31], NODE - 1, &mut out);
        assert_eq!(out, [2 << 1, 2 << 1, 1 << 1 | 1]);
        delta_decode(&mut out, NODE - 1);
        assert_eq!(out, [NODE, 0, NODE | 1 << 31]);

        let mut out = [0; 2];
        delta_encode(&[1 << 30, 0], 0, &mut out);
        assert_eq!(out, [((1 << 31) - 1) << 1, ((1 << 31) - 1) << 1]);
        delta_decode(&mut out, 0);
        assert_eq!(out, [1 << 30, 0]);
    }

    #[test]
    fn delta_round_trips() {
        let mut rng = SplitMix64::new(3);
        for len in 0..64 {
            for _ in 0..20 {
                let base = match rng.below(3) {
                    0 => NODE,
                    1 => 0,
                    _ => rng.next_u32(),
                };
                let words = words(&mut rng, base, len);
                let mut values = vec![0; len];
                delta_encode(&words, base, &mut values);
                assert!(values.iter().all(|&v| v >> 1 <= NODE));
                let mut slow = values.clone();
                delta_decode_scalar(&mut slow, base);
                delta_decode(&mut values, base);
                assert_eq!(values, words);
                assert_eq!(slow, words);
            }
        }
    }

    #[test]
    fn sorted_lists_from_their_base_are_small() {
        let mut rng = SplitMix64::new(5);
        let base = 1_000_000;
        let mut node = base;
        let words: Vec<u32> = (0..200)
            .map(|_| {
                node += 1 + rng.below(60) as u32;
                node | (rng.one_in(8) as u32) << 31
            })
            .collect();
        let mut values = vec![0; words.len()];
        delta_encode(&words, base, &mut values);
        assert!(values.iter().all(|&v| v < 1 << 8), "{values:?}");
        let mut bytes = Vec::new();
        assert_eq!(encode(&values, &mut bytes), 50 + 200);
    }

    #[test]
    fn edges_round_trip() {
        let mut rng = SplitMix64::new(9);
        let mut scratch = Vec::new();
        let mut stream = Vec::new();
        let mut lists = Vec::new();
        for len in (0..300).chain([127, 128, 16_383, 16_384]) {
            let base = rng.below(1 << 31) as u32;
            let words = words(&mut rng, base, len);
            let before = stream.len();
            let written = encode_edges(&words, base, &mut scratch, &mut stream);
            assert_eq!(written, stream.len() - before);
            let count_len = if len < 128 { 1 } else if len < 16_384 { 2 } else { 3 };
            assert_eq!(written, count_len + encoded_len(&stream[before + count_len..], len).unwrap());
            lists.push((base, words, written));
        }
        let mut at = 0;
        let mut out = vec![0xdead_beef; 3];
        for (base, words, written) in &lists {
            assert_eq!(decode_edges(&stream[at..], *base, &mut out), Ok(*written));
            assert_eq!(&out, words);
            at += written;
        }
        assert_eq!(at, stream.len());
    }

    #[test]
    fn hot_paths_allocate_nothing() {
        let (_, used) = blaze_alloc_count::measure(|| std::hint::black_box(Vec::<u8>::with_capacity(1)));
        assert_eq!(used.allocations, 1, "the counting allocator is installed for salsa's tests");

        let mut rng = SplitMix64::new(13);
        let words = words(&mut rng, 40, 100);
        let mut scratch = Vec::new();
        let mut stream = Vec::new();
        let written = blaze_alloc_count::assert_at_most(2, "encode_edges into empty buffers", || {
            encode_edges(&words, 40, &mut scratch, &mut stream)
        });
        let mut values = vec![0; 100];
        let mut out = Vec::with_capacity(100);
        blaze_alloc_count::assert_at_most(0, "varint decoding", || {
            assert_eq!(decode(&stream[1..], &mut values), Ok(written - 1));
            assert_eq!(decode_scalar(&stream[1..], &mut values), Ok(written - 1));
            assert_eq!(encoded_len(&stream[1..], 100), Ok(written - 1));
            delta_decode(&mut values, 40);
            delta_encode(&words, 40, &mut values);
            assert_eq!(decode_edges(&stream, 40, &mut out), Ok(written));
            stream.clear();
            encode_edges(&words, 40, &mut scratch, &mut stream);
        });
        assert_eq!(out, words);
    }
}
