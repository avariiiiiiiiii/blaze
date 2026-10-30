//! [`ContentId::of_many`]: many short inputs hashed a SIMD lane each.
//!
//! Full blocks go through `blake3::platform`'s `hash_many`; each input's last block, of any
//! length, through [`compress_lanes`], eight at a time. `blake3::platform` is unstable, and
//! `compress_lanes` is ours: `of_many_is_of_for_each` holds both to the hasher at every length
//! and grouping, so a change in either fails there and not in an ID.

use blake3::{
    BLOCK_LEN, CHUNK_LEN, IncrementCounter,
    hazmat::ContextKey,
    platform::{self, Platform},
};

use crate::ContentId;

/// BLAKE3's block flags.
const CHUNK_START: u8 = 1;
const CHUNK_END: u8 = 2;
const ROOT: u8 = 8;
const DERIVE_KEY_MATERIAL: u8 = 64;

/// Blocks a one-chunk input has before its last: 0 for one of at most a block.
fn leading(len: usize) -> usize {
    len.saturating_sub(1) / BLOCK_LEN
}

pub(crate) fn of_many(key: &ContextKey, inputs: &[&[u8]]) -> Vec<ContentId> {
    let platform = Platform::detect();
    let words = platform::words_from_le_bytes_32(key);
    let mut out = vec![ContentId::default(); inputs.len()];
    let mut counts = [0usize; CHUNK_LEN / BLOCK_LEN];
    for input in inputs {
        if input.len() <= CHUNK_LEN {
            counts[leading(input.len())] += 1;
        }
    }
    let mut starts = [0usize; CHUNK_LEN / BLOCK_LEN + 1];
    for (group, count) in counts.iter().enumerate() {
        starts[group + 1] = starts[group] + count;
    }
    let mut order = vec![0u32; starts[counts.len()]];
    let mut next = starts;
    for (at, input) in inputs.iter().enumerate() {
        if input.len() <= CHUNK_LEN {
            let group = leading(input.len());
            order[next[group]] = at as u32;
            next[group] += 1;
        } else {
            let mut hasher = blake3::Hasher::new_from_context_key(key);
            hasher.update(input);
            out[at] = ContentId(*hasher.finalize().as_bytes());
        }
    }
    let mut cvs = Vec::new();
    for group in 0..counts.len() {
        let members = &order[starts[group]..starts[group + 1]];
        if members.is_empty() {
            continue;
        }
        cvs.clear();
        if group == 0 {
            cvs.resize(members.len(), words);
        } else {
            leading_blocks(platform, &words, inputs, members, group, &mut cvs);
        }
        let first = if group == 0 { CHUNK_START } else { 0 };
        let flags = u32::from(DERIVE_KEY_MATERIAL | first | CHUNK_END | ROOT);
        for (batch, batch_cvs) in members.chunks(LANES).zip(cvs.chunks(LANES)) {
            let mut cv = [[0; LANES]; 8];
            let mut message = [[0; LANES]; 16];
            let mut block_len = [0; LANES];
            for (lane, (&at, words)) in batch.iter().zip(batch_cvs).enumerate() {
                let last = &inputs[at as usize][group * BLOCK_LEN..];
                block_len[lane] = last.len() as u32;
                for (word, &value) in words.iter().enumerate() {
                    cv[word][lane] = value;
                }
                let mut block = [0; BLOCK_LEN];
                block[..last.len()].copy_from_slice(last);
                for (word, bytes) in block.chunks_exact(4).enumerate() {
                    message[word][lane] = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                }
            }
            let root = compress_lanes(&cv, &message, block_len, flags);
            for (lane, &at) in batch.iter().enumerate() {
                let mut id = [0; 32];
                for (word, bytes) in id.chunks_exact_mut(4).enumerate() {
                    bytes.copy_from_slice(&root[word][lane].to_le_bytes());
                }
                out[at as usize] = ContentId(id);
            }
        }
    }
    out
}

/// Last blocks compressed together, a lane each: one AVX2 register of `u32`s, two NEON ones.
const LANES: usize = 8;

type Lanes = [u32; LANES];

const IV: [u32; 4] = [0x6A09_E667, 0xBB67_AE85, 0x3C6E_F372, 0xA54F_F53A];
const SCHEDULE: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];

#[inline(always)]
fn add(a: Lanes, b: Lanes) -> Lanes {
    let mut out = a;
    for lane in 0..LANES {
        out[lane] = out[lane].wrapping_add(b[lane]);
    }
    out
}

#[inline(always)]
fn xor_rotate(a: Lanes, b: Lanes, by: u32) -> Lanes {
    let mut out = a;
    for lane in 0..LANES {
        out[lane] = (out[lane] ^ b[lane]).rotate_right(by);
    }
    out
}

#[inline(always)]
fn g(v: &mut [Lanes; 16], [a, b, c, d]: [usize; 4], x: Lanes, y: Lanes) {
    v[a] = add(add(v[a], v[b]), x);
    v[d] = xor_rotate(v[d], v[a], 16);
    v[c] = add(v[c], v[d]);
    v[b] = xor_rotate(v[b], v[c], 12);
    v[a] = add(add(v[a], v[b]), y);
    v[d] = xor_rotate(v[d], v[a], 8);
    v[c] = add(v[c], v[d]);
    v[b] = xor_rotate(v[b], v[c], 7);
}

/// BLAKE3's compression of one block of chunk 0 in each of [`LANES`] lanes, each with its own
/// block length: what `hash_many` can't do, since it gives every lane full blocks. Transposed
/// (`message[w]` is word `w` of every lane), so each step is one instruction over the lanes
/// once LLVM vectorizes the loops; safe code, so no SIMD of our own to prove. Gives the
/// chaining value, the first 32 bytes of the output when `flags` has `ROOT`.
fn compress_lanes(cv: &[Lanes; 8], message: &[Lanes; 16], block_len: Lanes, flags: u32) -> [Lanes; 8] {
    let mut v = [[0; LANES]; 16];
    v[..8].copy_from_slice(cv);
    for (word, iv) in IV.iter().enumerate() {
        v[8 + word] = [*iv; LANES];
    }
    v[14] = block_len;
    v[15] = [flags; LANES];
    let mut m = *message;
    for round in 0..7 {
        g(&mut v, [0, 4, 8, 12], m[0], m[1]);
        g(&mut v, [1, 5, 9, 13], m[2], m[3]);
        g(&mut v, [2, 6, 10, 14], m[4], m[5]);
        g(&mut v, [3, 7, 11, 15], m[6], m[7]);
        g(&mut v, [0, 5, 10, 15], m[8], m[9]);
        g(&mut v, [1, 6, 11, 12], m[10], m[11]);
        g(&mut v, [2, 7, 8, 13], m[12], m[13]);
        g(&mut v, [3, 4, 9, 14], m[14], m[15]);
        if round < 6 {
            m = SCHEDULE.map(|word| m[word]);
        }
    }
    let mut out = [[0; LANES]; 8];
    for (word, lanes) in out.iter_mut().enumerate() {
        for lane in 0..LANES {
            lanes[lane] = v[word][lane] ^ v[word + 8][lane];
        }
    }
    out
}

/// The chaining value after the first `blocks` blocks of each of `members`, a SIMD lane
/// each, pushed onto `cvs` in their order.
fn leading_blocks(
    platform: Platform,
    key: &[u32; 8],
    inputs: &[&[u8]],
    members: &[u32],
    blocks: usize,
    cvs: &mut Vec<[u32; 8]>,
) {
    macro_rules! by_blocks {
        ($($blocks:literal)*) => {
            match blocks {
                $($blocks => hash_leading::<{ $blocks * BLOCK_LEN }>(platform, key, inputs, members, cvs),)*
                _ => unreachable!("a one-chunk input has at most 15 blocks before its last"),
            }
        };
    }
    by_blocks!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15)
}

fn hash_leading<const N: usize>(
    platform: Platform,
    key: &[u32; 8],
    inputs: &[&[u8]],
    members: &[u32],
    cvs: &mut Vec<[u32; 8]>,
) {
    let prefixes: Vec<&[u8; N]> = members
        .iter()
        .map(|&at| inputs[at as usize][..N].try_into().expect("a member has more than N bytes"))
        .collect();
    let mut bytes = vec![0; members.len() * blake3::OUT_LEN];
    platform.hash_many(
        &prefixes,
        key,
        0,
        IncrementCounter::No,
        DERIVE_KEY_MATERIAL,
        CHUNK_START,
        0,
        &mut bytes,
    );
    cvs.extend(bytes.chunks_exact(blake3::OUT_LEN).map(|cv| {
        platform::words_from_le_bytes_32(cv.try_into().expect("a 32-byte chaining value"))
    }));
}
