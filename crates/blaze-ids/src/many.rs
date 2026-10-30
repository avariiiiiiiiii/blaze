//! [`ContentId::of_many`]: many short inputs hashed a SIMD lane each.
//!
//! A one-chunk input's blocks before its last go through the backend's [`Leading`] (official
//! BLAKE3's `hash_many`, or [`leading_lanes`] under `blake3-servil`, which exports no kernel
//! that stops short of a root); each input's last block, of any length, through
//! [`compress_lanes`], eight at a time. `compress_lanes` is ours: `of_many_is_of_for_each` and
//! the backends' differential test hold every path to the hasher at every length and grouping,
//! so a change in any fails there and not in an ID.

use std::cell::RefCell;

use crate::{
    ContentId,
    b3::{self, BLOCK_LEN, CHUNK_LEN, ContextKey},
    radix,
};

/// How a backend hashes what [`compress_lanes`] doesn't: a one-chunk input's blocks before its
/// last, and an input longer than a chunk.
pub(crate) trait Leading {
    /// The chaining value after the first `blocks` blocks (1 to 15) of each of `members`, a
    /// SIMD lane each, pushed onto `cvs` in their order.
    fn leading_blocks(key: &[u32; 8], inputs: &[&[u8]], members: &[u32], blocks: usize, cvs: &mut Vec<[u32; 8]>);

    /// The derived key of `input`, longer than a chunk, under the context key `key`.
    fn long(key: &ContextKey, input: &[u8]) -> [u8; 32];
}

/// A thread's buffers for [`of_many_into`], kept warm across calls: the inputs grouped by
/// leading blocks, the grouping's cursors and one group's chaining values.
struct Kept {
    groups: radix::Csr<u32>,
    cursors: Vec<u32>,
    cvs: Vec<[u32; 8]>,
}

thread_local! {
    static KEPT: RefCell<Kept> = const {
        RefCell::new(Kept {
            groups: radix::Csr::new(),
            cursors: Vec::new(), // alloc: empty until a thread's first call, then reused
            cvs: Vec::new(),     // alloc: empty until a thread's first call, then reused
        })
    };
}

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
    // alloc: the result, which the caller keeps; `of_many_into` fills one of the caller's
    let mut out = Vec::with_capacity(inputs.len());
    of_many_into::<b3::Many>(key, inputs, &mut out);
    out
}

/// [`of_many`] by the backend `L`, into `out`, cleared first: a warm call with room allocates
/// nothing.
pub(crate) fn of_many_into<L: Leading>(key: &ContextKey, inputs: &[&[u8]], out: &mut Vec<ContentId>) {
    out.clear();
    out.resize(inputs.len(), ContentId::default());
    KEPT.with_borrow_mut(|kept| hash_into::<L>(key, inputs, out, kept));
}

/// `bytes`' eight little-endian words.
fn words_of(bytes: &[u8; 32]) -> [u32; 8] {
    std::array::from_fn(|word| {
        u32::from_le_bytes([bytes[4 * word], bytes[4 * word + 1], bytes[4 * word + 2], bytes[4 * word + 3]])
    })
}

fn hash_into<L: Leading>(key: &ContextKey, inputs: &[&[u8]], out: &mut [ContentId], kept: &mut Kept) {
    let words = words_of(key);
    let Kept { groups, cursors, cvs } = kept;
    groups.rebuild(
        CHUNK_LEN / BLOCK_LEN,
        inputs
            .iter()
            .enumerate()
            .filter(|(_, input)| input.len() <= CHUNK_LEN)
            .map(|(at, input)| (leading(input.len()) as u32, at as u32)),
        cursors,
    );
    for (at, input) in inputs.iter().enumerate() {
        if input.len() > CHUNK_LEN {
            out[at] = ContentId(L::long(key, input));
        }
    }
    for group in 0..groups.rows() {
        let members = groups.row(group);
        if members.is_empty() {
            continue;
        }
        cvs.clear();
        if group == 0 {
            cvs.resize(members.len(), words);
        } else {
            L::leading_blocks(&words, inputs, members, group, cvs);
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

/// [`Leading::leading_blocks`] by [`compress_lanes`]: [`LANES`] members at a time, a block of
/// each per compression, for a backend with no kernel that stops short of a chunk's root.
#[cfg_attr(
    not(all(
        target_endian = "little",
        any(
            all(target_arch = "x86_64", target_os = "linux"),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")),
        ),
    )),
    expect(dead_code, reason = "blake3-servil's way alone, and it doesn't build for this target")
)]
pub(crate) fn leading_lanes(key: &[u32; 8], inputs: &[&[u8]], members: &[u32], blocks: usize, cvs: &mut Vec<[u32; 8]>) {
    let flags = u32::from(DERIVE_KEY_MATERIAL);
    for batch in members.chunks(LANES) {
        // Lanes past the batch hash zeros; only `..batch.len()` is kept.
        let mut cv: [Lanes; 8] = key.map(|word| [word; LANES]);
        for block in 0..blocks {
            let mut message = [[0; LANES]; 16];
            for (lane, &at) in batch.iter().enumerate() {
                let bytes = &inputs[at as usize][block * BLOCK_LEN..(block + 1) * BLOCK_LEN];
                for (word, value) in bytes.chunks_exact(4).enumerate() {
                    message[word][lane] = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
                }
            }
            let first = if block == 0 { u32::from(CHUNK_START) } else { 0 };
            cv = compress_lanes(&cv, &message, [BLOCK_LEN as u32; LANES], flags | first);
        }
        cvs.extend((0..batch.len()).map(|lane| std::array::from_fn(|word| cv[word][lane])));
    }
}
