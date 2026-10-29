//! Content-defined chunking (FastCDC, normalized): cut points depend on the bytes around them,
//! not on their offset, so two outputs that differ in a few places share every chunk but the
//! few around the differences, whatever was inserted or removed before them.

use std::ops::Range;

pub(crate) const MIN: usize = 16 << 10;
pub(crate) const AVERAGE: usize = 64 << 10;
pub(crate) const MAX: usize = 256 << 10;

/// Before [`AVERAGE`] a cut needs more of the hash's top bits zero (18), after it fewer (14),
/// which keeps chunk sizes near the average.
const MASK_SMALL: u64 = !0 << (64 - 18);
const MASK_LARGE: u64 = !0 << (64 - 14);

/// The gear table: 256 fixed random words (SplitMix64 from a fixed seed). Part of the format:
/// changing it changes every chunk and so every chunk key.
const GEAR: [u64; 256] = gear();

const fn gear() -> [u64; 256] {
    let mut out = [0; 256];
    let mut state: u64 = 0x626c_617a_6520_7632;
    let mut at = 0;
    while at < 256 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        out[at] = z ^ (z >> 31);
        at += 1;
    }
    out
}

/// Where the chunk that starts `bytes` ends.
fn cut(bytes: &[u8]) -> usize {
    if bytes.len() <= MIN {
        return bytes.len();
    }
    let end = bytes.len().min(MAX);
    let middle = end.min(AVERAGE);
    let mut hash: u64 = 0;
    let mut at = MIN;
    while at < middle {
        hash = (hash << 1).wrapping_add(GEAR[bytes[at] as usize]);
        if hash & MASK_SMALL == 0 {
            return at + 1;
        }
        at += 1;
    }
    while at < end {
        hash = (hash << 1).wrapping_add(GEAR[bytes[at] as usize]);
        if hash & MASK_LARGE == 0 {
            return at + 1;
        }
        at += 1;
    }
    end
}

/// The chunks of `bytes`, in order, covering all of it: each at most [`MAX`], each but the last
/// at least [`MIN`].
pub(crate) fn chunks(bytes: &[u8]) -> Vec<Range<usize>> {
    let mut out = Vec::with_capacity(bytes.len() / AVERAGE + 1);
    let mut start = 0;
    while start < bytes.len() {
        let len = cut(&bytes[start..]);
        out.push(start..start + len);
        start += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut rng = blaze_core::sim::Rng::new(seed);
        (0..len).map(|_| rng.next_u64() as u8).collect()
    }

    #[test]
    fn chunks_cover_everything_within_bounds() {
        for (len, seed) in [(0, 1), (1, 2), (MIN, 3), (MIN + 1, 4), (3 << 20, 5), (MAX * 3 + 7, 6)] {
            let bytes = noise(len, seed);
            let chunks = chunks(&bytes);
            let mut at = 0;
            for (n, chunk) in chunks.iter().enumerate() {
                assert_eq!(chunk.start, at);
                assert!(chunk.len() <= MAX);
                assert!(chunk.len() >= MIN || n + 1 == chunks.len(), "{len}: chunk {n}");
                at = chunk.end;
            }
            assert_eq!(at, len);
        }
    }

    #[test]
    fn chunks_average_near_the_average() {
        let bytes = noise(16 << 20, 9);
        let count = chunks(&bytes).len();
        let average = bytes.len() / count;
        assert!((AVERAGE / 2..AVERAGE * 2).contains(&average), "average {average}");
    }

    /// An insertion near the start changes only the chunks around it.
    #[test]
    fn an_insertion_shares_the_chunks_after_it() {
        let bytes = noise(4 << 20, 11);
        let mut edited = bytes.clone();
        edited.splice(1000..1000, *b"an insertion").for_each(drop);
        let set = |bytes: &[u8]| -> HashSet<Vec<u8>> {
            chunks(bytes).into_iter().map(|it| bytes[it].to_vec()).collect()
        };
        let (before, after) = (set(&bytes), set(&edited));
        let shared = before.intersection(&after).count();
        assert!(shared + 3 >= before.len(), "{shared} of {} shared", before.len());
    }

    #[test]
    fn the_same_bytes_cut_the_same() {
        let bytes = noise(2 << 20, 13);
        assert_eq!(chunks(&bytes), chunks(&bytes.clone()));
    }
}
