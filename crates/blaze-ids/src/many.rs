//! [`ContentId::of_many`]: many short inputs hashed a SIMD lane each.
//!
//! It drives `blake3::platform`, which blake3 marks unstable: `of_many_is_of_for_each`
//! holds it to the hasher at every length and grouping, so a blake3 that changes it fails
//! there and not in an ID.

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
        for (&at, cv) in members.iter().zip(&mut cvs) {
            let input = inputs[at as usize];
            let last = &input[group * BLOCK_LEN..];
            let mut block = [0; BLOCK_LEN];
            block[..last.len()].copy_from_slice(last);
            let flags = DERIVE_KEY_MATERIAL | first | CHUNK_END | ROOT;
            platform.compress_in_place(cv, &block, last.len() as u8, 0, flags);
            out[at as usize] = ContentId(platform::le_bytes_from_words_32(cv));
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
