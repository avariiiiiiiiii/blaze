//! The database's edges: one CSR array of `u32` words, each a node and the edge's kind.
//!
//! A memo's edges are a range of this array (`EdgeSpan`), rustc's one edge list for the whole
//! graph (`SerializedDepGraph`), so a memo's edges cost 4 bytes each and no allocation. The array
//! is pages of words that double and never move, and a range never crosses a page, so a memo's
//! edges are one slice. A task reserves 256 words at a time (one compare-and-swap per 256 edges)
//! and writes a query's edges when it finishes; the memo's publication orders them for readers
//! (docs/v2/plan/design/concurrency.md, "Dependencies are recorded per task").
//!
//! Replaced memos leave dead ranges; `Graph::compact` copies the live ones at a new revision, with
//! `&mut`, into a [`Settled`] region: each edge bit-packed at the width the largest node needs
//! (⌈log2 nodes⌉ + 1 bits, docs/v2/plan/lanes/db/speed-of-light.md, "Edges at rest"). Positions
//! below the settled length are settled edges; the fresh array starts there (its `base`), so a
//! span says which form it is in by its start alone and stays 8 bytes.

use std::cell::Cell;

use crate::sync::OnceLock;
use crate::sync::atomic::{AtomicU32, Ordering};

use super::OUTPUT;

/// Page `p` holds `1 << (FIRST_PAGE_BITS + p)` words; 20 pages hold all but the last 4096 of
/// 2^32.
const FIRST_PAGE_BITS: u32 = 12;
const PAGES: usize = 20;

/// The words of every page together.
const CAPACITY: u64 = page_start(PAGES);

/// Words a task reserves at a time. A query with more than half this many edges reserves its
/// own range, so a chunk wastes at most half itself.
pub(crate) const CHUNK: u32 = 256;

/// A memo's edges: `len` words from `start`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct EdgeSpan {
    pub(crate) start: u32,
    pub(crate) len: u32,
}

impl EdgeSpan {
    pub(crate) const EMPTY: EdgeSpan = EdgeSpan { start: 0, len: 0 };
}

/// The part of the array a task reserved and has not filled.
#[derive(Copy, Clone, Default)]
pub(crate) struct Chunk {
    /// The [`Edges::generation`] it was reserved from; 0, which none has, before the first.
    generation: u32,
    next: u32,
    end: u32,
}

pub(crate) struct Edges {
    pages: Box<[OnceLock<super::Words<AtomicU32>>]>,
    /// The words reserved: every range starts below `base` plus it.
    len: AtomicU32,
    /// Which array this is; a compaction makes the next, and a task's chunk of another is
    /// dropped.
    generation: u32,
    /// The position of word 0: the settled edges are the positions below it.
    base: u32,
}

const fn page_start(page: usize) -> u64 {
    (1 << (FIRST_PAGE_BITS as u64 + page as u64)) - (1 << FIRST_PAGE_BITS)
}

const fn page_len(page: usize) -> u64 {
    1 << (FIRST_PAGE_BITS as u64 + page as u64)
}

/// The page of `position` and its offset there.
#[inline]
fn locate(position: u64) -> (usize, usize) {
    let x = position + (1 << FIRST_PAGE_BITS);
    let bits = u64::BITS - 1 - x.leading_zeros();
    (
        (bits - FIRST_PAGE_BITS) as usize,
        (x - (1 << bits)) as usize,
    )
}

/// The first position at or after `start` from which `n` words lie in one page.
fn fit(start: u64, n: u64) -> u64 {
    let (mut page, offset) = locate(start);
    if offset as u64 + n <= page_len(page) {
        return start;
    }
    page += 1;
    while page_len(page) < n {
        page += 1;
    }
    page_start(page)
}

impl Edges {
    /// An empty array whose word 0 is position `base`, the first past the settled edges.
    pub(crate) fn new(generation: u32, base: u32) -> Edges {
        assert_ne!(generation, 0, "salsa: edge array generation 0 marks a task's empty chunk");
        Edges {
            // alloc: setup, once per database and compaction: the edge array's page directory
            pages: (0..PAGES).map(|_| OnceLock::new()).collect(),
            len: AtomicU32::new(0),
            generation,
            base,
        }
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }

    /// The words reserved so far, live or dead.
    pub(crate) fn len(&self) -> u32 {
        self.len.load(Ordering::Relaxed)
    }

    /// Reserves `n` words in one page and returns the position of the first.
    pub(crate) fn reserve(&self, n: u32) -> u32 {
        let n = u64::from(n);
        let mut start = self.len.load(Ordering::Relaxed);
        loop {
            let begin = fit(u64::from(start), n);
            let end = begin + n;
            assert!(
                end <= CAPACITY && u64::from(self.base) + end < 1 << 32,
                "salsa: the edge array is full (2^32 positions, settled edges and words reserved \
                 since the last compaction)"
            );
            match self.len.compare_exchange_weak(
                start,
                end as u32,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    let (page, _) = locate(begin);
                    self.page(page);
                    return self.base + begin as u32;
                }
                Err(now) => start = now,
            }
        }
    }

    /// Reserves `n` words from the task's chunk, reserving a new one when it is short.
    pub(crate) fn reserve_for_task(&self, chunk: &Cell<Chunk>, n: u32) -> u32 {
        let current = chunk.get();
        if current.generation == self.generation && current.end - current.next >= n {
            chunk.set(Chunk {
                next: current.next + n,
                ..current
            });
            return current.next;
        }
        if n > CHUNK / 2 {
            return self.reserve(n);
        }
        let start = self.reserve(CHUNK);
        chunk.set(Chunk {
            generation: self.generation,
            next: start + n,
            end: start + CHUNK,
        });
        start
    }

    fn page(&self, page: usize) -> &[AtomicU32] {
        self.pages[page].get_or_init(|| super::zeroed_u32s(page_len(page) as usize))
    }

    /// The words of `span`, which [`Edges::reserve`] or [`Edges::reserve_for_task`] gave.
    #[inline]
    pub(crate) fn slice(&self, span: EdgeSpan) -> &[AtomicU32] {
        if span.len == 0 {
            return &[];
        }
        let index = span
            .start
            .checked_sub(self.base)
            .expect("salsa: a span of settled edges read as words");
        let (page, offset) = locate(u64::from(index));
        let page = self.pages[page]
            .get()
            .expect("salsa: a memo's edges lie in a page their reservation allocated");
        &page[offset..offset + span.len as usize]
    }
}

/// Bytes past a settled region's last edge, so the 8-byte load at any edge's first byte and an
/// 8-edge unpack's loads (40 bytes from a group's first byte, `simd::READ`) stay inside it.
const PAD: usize = 64;

/// The bits a settled edge takes when `nodes` keys have a node: the largest node's,
/// ⌈log2 nodes⌉, and one for the output bit; 1 to 32.
pub(crate) fn width_for(nodes: u32) -> u32 {
    u32::BITS - nodes.saturating_sub(1).leading_zeros() + 1
}

/// A word as a settled edge: the node above the output bit, `width` bits.
#[inline]
fn pack(word: u32) -> u64 {
    u64::from(word.rotate_left(1))
}

/// The word of the settled edge in the low `width` bits of `bits`.
#[inline]
fn unpack(bits: u64, width: u32) -> u32 {
    ((bits & ((1u64 << width) - 1)) as u32).rotate_right(1)
}

/// The edges a compaction settled: edge `i` is `width` bits at bit `i * width`, least significant
/// bit first, as [`pack`] makes it. Its positions are `0..len`; the fresh array starts at `len`.
///
/// Only [`SettledWriter`] writes the bytes, inside a compaction, which holds `&mut Graph`; a
/// reader borrows `&Graph`, so the bytes never change while any reader can see them, and reads
/// are plain loads with no ordering of their own.
pub(crate) struct Settled {
    /// The packed edges then at least [`PAD`] zero bytes; `None` while there are no edges.
    bytes: Option<super::Words<u8>>,
    len: u32,
    width: u32,
}

impl Settled {
    pub(crate) fn empty() -> Settled {
        Settled {
            bytes: None,
            len: 0,
            width: 1,
        }
    }

    /// How many edges are settled: the positions below it.
    #[inline]
    pub(crate) fn len(&self) -> u32 {
        self.len
    }

    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    /// The bytes the edges take, padding aside.
    pub(crate) fn packed_bytes(&self) -> u64 {
        (u64::from(self.len) * u64::from(self.width)).div_ceil(8)
    }

    /// Whether `span` names settled edges. A span never straddles: a settled one ends at or
    /// below `len`, a fresh one starts at or above it.
    #[inline]
    pub(crate) fn holds(&self, span: EdgeSpan) -> bool {
        span.start < self.len
    }

    /// The edges of a span [`Settled::holds`].
    #[inline]
    pub(crate) fn view(&self, span: EdgeSpan) -> Packed<'_> {
        let bytes = self
            .bytes
            .as_deref()
            .expect("salsa: a settled span lies in a settled region");
        assert!(
            u64::from(span.start) + u64::from(span.len) <= u64::from(self.len),
            "salsa: a settled span ends past the settled edges"
        );
        Packed {
            bytes,
            bit: u64::from(span.start) * u64::from(self.width),
            len: span.len,
            width: self.width,
        }
    }
}

/// A span of settled edges, read in place.
#[derive(Copy, Clone)]
pub(crate) struct Packed<'a> {
    /// The whole region, padding included, so every load below stays in it.
    bytes: &'a [u8],
    /// The bit of the span's first edge.
    bit: u64,
    len: u32,
    width: u32,
}

impl<'a> Packed<'a> {
    #[inline]
    pub(crate) fn len(self) -> u32 {
        self.len
    }

    /// The word at `position`, which is below `len`: one 8-byte load, a shift and a mask.
    #[inline]
    pub(crate) fn word(self, position: u32) -> u32 {
        debug_assert!(position < self.len);
        let bit = self.bit + u64::from(position) * u64::from(self.width);
        let byte = (bit >> 3) as usize;
        let mut chunk = [0u8; 8];
        chunk.copy_from_slice(&self.bytes[byte..byte + 8]);
        unpack(u64::from_le_bytes(chunk) >> (bit & 7), self.width)
    }

    /// The words of group `group` (`8 * group..8 * group + 8`, below `len`), one load each.
    #[inline]
    fn unpack8(self, group: u32, out: &mut [u32; 8]) {
        for (k, out) in (group * 8..).zip(out.iter_mut()) {
            *out = self.word(k);
        }
    }

    #[inline]
    pub(crate) fn iter(self) -> PackedIter<'a> {
        PackedIter {
            packed: self,
            front: 0,
            back: self.len,
            group: u32::MAX,
            buf: [0; 8],
            #[cfg(all(
                not(feature = "shuttle"),
                not(miri),
                target_endian = "little",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            lanes: (self.len >= simd::MIN && simd::available())
                .then(|| simd::Lanes::new((self.bit & 7) as u32, self.width)),
        }
    }
}

/// The words of a [`Packed`] in order, unpacked 8 at a time (with SIMD on x86_64 with AVX2 and
/// on aarch64, for spans of at least 32 edges, `simd::MIN`). A position in the last, partial
/// group is read alone.
#[derive(Clone)]
pub(crate) struct PackedIter<'a> {
    packed: Packed<'a>,
    front: u32,
    back: u32,
    /// The group `buf` holds; `u32::MAX`, no group's, for none.
    group: u32,
    buf: [u32; 8],
    #[cfg(all(
        not(feature = "shuttle"),
        not(miri),
        target_endian = "little",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    lanes: Option<simd::Lanes>,
}

impl PackedIter<'_> {
    #[inline]
    fn at(&mut self, position: u32) -> u32 {
        let group = position / 8;
        if group != self.group {
            if group * 8 + 8 > self.packed.len {
                return self.packed.word(position);
            }
            self.fill(group);
        }
        self.buf[(position % 8) as usize]
    }

    #[inline]
    fn fill(&mut self, group: u32) {
        self.group = group;
        #[cfg(all(
            not(feature = "shuttle"),
            not(miri),
            target_endian = "little",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        if let Some(lanes) = &self.lanes {
            let byte = (self.packed.bit >> 3) as usize
                + group as usize * self.packed.width as usize;
            assert!(byte + simd::READ <= self.packed.bytes.len());
            // SAFETY: `lanes` is set only where `simd::available()` said the CPU has the
            // instructions `unpack8` uses, and the assert keeps its `READ` bytes from `byte`
            // inside `bytes`, which the region's padding guarantees for a whole group.
            unsafe {
                simd::unpack8(
                    self.packed.bytes.as_ptr().add(byte),
                    lanes,
                    self.packed.width,
                    &mut self.buf,
                );
            }
            return;
        }
        self.packed.unpack8(group, &mut self.buf);
    }
}

impl Iterator for PackedIter<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        if self.front == self.back {
            return None;
        }
        let position = self.front;
        self.front += 1;
        Some(self.at(position))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = (self.back - self.front) as usize;
        (n, Some(n))
    }
}

impl DoubleEndedIterator for PackedIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<u32> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        Some(self.at(self.back))
    }
}

/// Unpacking 8 settled edges at once. The 8 edges of a group are `width` whole bytes, so every
/// group of a span starts at the same bit of a byte and the lanes' byte offsets and shifts are
/// computed once per span: per group, four 16-byte loads, a byte shuffle putting each edge's 8
/// bytes in a 64-bit lane, a shift and a mask per lane, then narrowing to 8 words.
#[cfg(all(
    not(feature = "shuttle"),
    not(miri),
    target_endian = "little",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod simd {
    /// Spans shorter than this unpack with scalar loads: setting up the lanes costs about what
    /// unpacking a group does.
    pub(super) const MIN: u32 = 32;

    /// The bytes an unpack reads from a group's first byte: 16 from the last pair's offset,
    /// at most `(7 + 6 * 32) / 8 = 24`.
    pub(super) const READ: usize = 40;

    #[derive(Copy, Clone)]
    pub(super) struct Lanes {
        /// The byte each pair of edges (0 and 1, 2 and 3, ...) is loaded from.
        load: [u8; 4],
        /// Per pair, the bytes of the load each of its two 64-bit lanes takes.
        shuffle: [[u8; 16]; 4],
        /// Per edge, its first bit in its lane; negated on aarch64, whose vector shift shifts
        /// right by a negative count.
        shift: [i64; 8],
    }

    impl Lanes {
        /// The lanes of a span whose first edge starts at bit `first_bit` (0 to 7) of a byte.
        pub(super) fn new(first_bit: u32, width: u32) -> Lanes {
            let mut offset = [0u8; 8];
            let mut shift = [0i64; 8];
            for (k, (offset, shift)) in offset.iter_mut().zip(shift.iter_mut()).enumerate() {
                let bit = first_bit as usize + k * width as usize;
                *offset = (bit / 8) as u8;
                let right = (bit % 8) as i64;
                *shift = if cfg!(target_arch = "aarch64") {
                    -right
                } else {
                    right
                };
            }
            let mut shuffle = [[0u8; 16]; 4];
            for (pair, bytes) in shuffle.iter_mut().enumerate() {
                let second = offset[2 * pair + 1] - offset[2 * pair];
                for j in 0..8u8 {
                    bytes[usize::from(j)] = j;
                    bytes[usize::from(8 + j)] = second + j;
                }
            }
            Lanes {
                load: [offset[0], offset[2], offset[4], offset[6]],
                shuffle,
                shift,
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn available() -> bool {
        std::arch::is_x86_feature_detected!("avx2")
    }

    #[cfg(target_arch = "aarch64")]
    pub(super) fn available() -> bool {
        true
    }

    /// Unpacks the group whose first byte is at `group` into `out`.
    ///
    /// # Safety
    ///
    /// The CPU has AVX2, and the [`READ`] bytes from `group` are readable.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn unpack8(group: *const u8, lanes: &Lanes, width: u32, out: &mut [u32; 8]) {
        use std::arch::x86_64::*;
        // SAFETY: each load is 16 bytes from `lanes.load[pair] <= 24` past `group`, inside the
        // `READ` bytes the caller keeps readable; the masks and shifts are loaded from `lanes`'s
        // arrays (64 bytes each, two 32-byte loads), the store is to `out`'s 32 bytes, and
        // every load and store is unaligned. AVX2 is the caller's to check.
        unsafe {
            let low = _mm256_set_m128i(
                _mm_loadu_si128(group.add(usize::from(lanes.load[1])).cast::<__m128i>()),
                _mm_loadu_si128(group.add(usize::from(lanes.load[0])).cast::<__m128i>()),
            );
            let high = _mm256_set_m128i(
                _mm_loadu_si128(group.add(usize::from(lanes.load[3])).cast::<__m128i>()),
                _mm_loadu_si128(group.add(usize::from(lanes.load[2])).cast::<__m128i>()),
            );
            let shuffle = lanes.shuffle.as_ptr().cast::<__m256i>();
            let shift = lanes.shift.as_ptr().cast::<__m256i>();
            let mask = _mm256_set1_epi64x(((1u64 << width) - 1) as i64);
            let low = _mm256_and_si256(
                _mm256_srlv_epi64(
                    _mm256_shuffle_epi8(low, _mm256_loadu_si256(shuffle)),
                    _mm256_loadu_si256(shift),
                ),
                mask,
            );
            let high = _mm256_and_si256(
                _mm256_srlv_epi64(
                    _mm256_shuffle_epi8(high, _mm256_loadu_si256(shuffle.add(1))),
                    _mm256_loadu_si256(shift.add(1)),
                ),
                mask,
            );
            // 32-bit lanes 0, 2, 4, 6 hold edges 0 to 3 and lanes 1, 3, 5, 7 edges 4 to 7.
            let words = _mm256_permutevar8x32_epi32(
                _mm256_or_si256(low, _mm256_slli_epi64::<32>(high)),
                _mm256_setr_epi32(0, 2, 4, 6, 1, 3, 5, 7),
            );
            let words = _mm256_or_si256(
                _mm256_srli_epi32::<1>(words),
                _mm256_slli_epi32::<31>(words),
            );
            _mm256_storeu_si256(out.as_mut_ptr().cast::<__m256i>(), words);
        }
    }

    /// Unpacks the group whose first byte is at `group` into `out`.
    ///
    /// # Safety
    ///
    /// The [`READ`] bytes from `group` are readable.
    #[cfg(target_arch = "aarch64")]
    #[inline]
    pub(super) unsafe fn unpack8(group: *const u8, lanes: &Lanes, width: u32, out: &mut [u32; 8]) {
        use std::arch::aarch64::*;
        // SAFETY: each load is 16 bytes from `lanes.load[pair] <= 24` past `group`, inside the
        // `READ` bytes the caller keeps readable; the shuffles and shifts are loaded from
        // `lanes`'s arrays, the stores are to `out`'s 32 bytes, and NEON loads and stores need
        // no alignment. NEON is part of every aarch64 target.
        unsafe {
            let mask = vdupq_n_u64((1u64 << width) - 1);
            let mut pairs = [vdup_n_u32(0); 4];
            for (pair, narrowed) in pairs.iter_mut().enumerate() {
                let bytes = vqtbl1q_u8(
                    vld1q_u8(group.add(usize::from(lanes.load[pair]))),
                    vld1q_u8(lanes.shuffle[pair].as_ptr()),
                );
                let edges = vshlq_u64(
                    vreinterpretq_u64_u8(bytes),
                    vld1q_s64(lanes.shift.as_ptr().add(2 * pair)),
                );
                *narrowed = vmovn_u64(vandq_u64(edges, mask));
            }
            let low = vcombine_u32(pairs[0], pairs[1]);
            let high = vcombine_u32(pairs[2], pairs[3]);
            vst1q_u32(
                out.as_mut_ptr(),
                vorrq_u32(vshrq_n_u32::<1>(low), vshlq_n_u32::<31>(low)),
            );
            vst1q_u32(
                out.as_mut_ptr().add(4),
                vorrq_u32(vshrq_n_u32::<1>(high), vshlq_n_u32::<31>(high)),
            );
        }
    }
}

/// A settled region being written, edge after edge, by a compaction.
pub(crate) struct SettledWriter {
    /// Room for `capacity` edges and the padding; `None` before the first.
    bytes: Option<super::Words<u8>>,
    capacity: u32,
    len: u32,
    width: u32,
    /// The bytes written whole; the bits after them wait in `pending`.
    byte: usize,
    pending: u64,
    pending_bits: u32,
}

impl SettledWriter {
    /// A writer of edges `width` bits each, with room for `capacity` (the words the array held,
    /// live or dead, which bounds the live ones) before it must grow.
    pub(crate) fn new(width: u32, capacity: u32) -> SettledWriter {
        assert!((1..=32).contains(&width), "salsa: a settled edge is 1 to 32 bits");
        let mut writer = SettledWriter {
            bytes: None,
            capacity: 0,
            len: 0,
            width,
            byte: 0,
            pending: 0,
            pending_bits: 0,
        };
        writer.reserve(capacity.min(OUTPUT - 1));
        writer
    }

    pub(crate) fn len(&self) -> u32 {
        self.len
    }

    /// Makes room for `n` more edges, copying what is written into a larger region if needed.
    pub(crate) fn reserve(&mut self, n: u32) {
        let need = u64::from(self.len) + u64::from(n);
        if need <= u64::from(self.capacity) {
            return;
        }
        let need = u32::try_from(need)
            .ok()
            .filter(|&need| need < OUTPUT)
            .expect("salsa: 2^31 settled edges or more");
        let capacity = need
            .max(self.capacity.saturating_mul(2).min(OUTPUT - 1))
            .max(4096);
        let bytes = (u64::from(capacity) * u64::from(self.width)).div_ceil(8) as usize + PAD;
        let mut grown = zeroed_bytes(bytes);
        if let Some(old) = &self.bytes {
            grown[..self.byte].copy_from_slice(&old[..self.byte]);
        }
        self.bytes = Some(grown);
        self.capacity = capacity;
    }

    /// Appends the edge of `word`, after a [`SettledWriter::reserve`] that made room for it.
    #[inline]
    pub(crate) fn push(&mut self, word: u32) {
        assert!(self.len < self.capacity, "salsa: a settled edge pushed past its reservation");
        let packed = pack(word);
        assert!(
            packed >> self.width == 0,
            "salsa: edge word {word:#x} is wider than the {} bits compaction packs at",
            self.width
        );
        self.pending |= packed << self.pending_bits;
        self.pending_bits += self.width;
        if self.pending_bits >= 32 {
            let bytes = self
                .bytes
                .as_deref_mut()
                .expect("salsa: a writer with room has a region");
            bytes[self.byte..self.byte + 4].copy_from_slice(&(self.pending as u32).to_le_bytes());
            self.byte += 4;
            self.pending >>= 32;
            self.pending_bits -= 32;
        }
        self.len += 1;
    }

    pub(crate) fn finish(mut self) -> Settled {
        if self.len == 0 {
            return Settled {
                bytes: None,
                len: 0,
                width: self.width,
            };
        }
        let bytes = self
            .bytes
            .as_deref_mut()
            .expect("salsa: a writer with edges has a region");
        // The padding past the last edge has room for the 8 bytes.
        bytes[self.byte..self.byte + 8].copy_from_slice(&self.pending.to_le_bytes());
        Settled {
            bytes: self.bytes,
            len: self.len,
            width: self.width,
        }
    }
}

/// `len` zeroed bytes for a settled region.
#[cfg(not(feature = "shuttle"))]
fn zeroed_bytes(len: usize) -> super::Words<u8> {
    // SAFETY: all-zero bits are a `u8`, which needs no drop.
    unsafe { super::Zeroed::new(len) }
}

/// `len` zeroed bytes for a settled region.
#[cfg(feature = "shuttle")]
fn zeroed_bytes(len: usize) -> super::Words<u8> {
    // alloc: a settled region, once per compaction
    vec![0; len].into_boxed_slice()
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use std::cell::Cell;

    use super::{CAPACITY, CHUNK, Chunk, EdgeSpan, Edges, fit, locate, page_len, page_start};
    use crate::sync::atomic::Ordering;

    #[test]
    fn pages_tile_the_positions() {
        assert_eq!(locate(0), (0, 0));
        assert_eq!(locate(4095), (0, 4095));
        assert_eq!(locate(4096), (1, 0));
        assert_eq!(locate(page_start(5) + 7), (5, 7));
        assert_eq!(CAPACITY, (1 << 32) - 4096);
        assert_eq!(fit(4000, 96), 4000);
        assert_eq!(fit(4000, 97), 4096);
        assert_eq!(fit(4000, 20_000), page_start(3));
        assert!(page_len(3) >= 20_000);
    }

    /// The safe reference is a list of the ranges handed out: they are disjoint, each in one page.
    #[test]
    fn ranges_never_overlap_or_cross_a_page() {
        let edges = Edges::new(1, 0);
        let chunk = Cell::new(Chunk::default());
        let mut ranges = Vec::new();
        let mut state = 3u64;
        for _ in 0..5000 {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let n = match state >> 61 {
                0 => 0,
                1 => 300 + (state >> 40) as u32 % 3000,
                _ => 1 + (state >> 40) as u32 % 40,
            };
            if n == 0 {
                continue;
            }
            let start = edges.reserve_for_task(&chunk, n);
            let (first, _) = locate(u64::from(start));
            let (last, _) = locate(u64::from(start + n - 1));
            assert_eq!(first, last, "a range crosses a page");
            let slice = edges.slice(EdgeSpan { start, len: n });
            for word in slice {
                assert_eq!(word.load(Ordering::Relaxed), 0, "a range was handed out twice");
                word.store(1, Ordering::Relaxed);
            }
            ranges.push((start, n));
        }
        ranges.sort_unstable();
        for pair in ranges.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0);
        }
    }

    #[test]
    fn a_chunk_of_another_generation_is_dropped() {
        let old = Edges::new(1, 0);
        let chunk = Cell::new(Chunk::default());
        let first = old.reserve_for_task(&chunk, 3);
        assert_eq!(old.reserve_for_task(&chunk, 3), first + 3);
        assert_eq!(old.len(), CHUNK);
        let new = Edges::new(2, 1000);
        assert_eq!(new.reserve_for_task(&chunk, 3), 1000);
        assert_eq!(new.len(), CHUNK);
        assert_eq!(new.reserve(5), 1000 + CHUNK);
        let span = EdgeSpan {
            start: 1000 + CHUNK,
            len: 5,
        };
        new.slice(span)[4].store(9, Ordering::Relaxed);
        assert_eq!(new.slice(span)[4].load(Ordering::Relaxed), 9);
    }

    /// The SIMD unpack against the scalar reference, at every width and first bit, over random
    /// bytes.
    #[cfg(all(
        not(miri),
        target_endian = "little",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn simd_and_scalar_unpack_agree() {
        if !super::simd::available() {
            return;
        }
        let mut state = 0x853c_49e6_748f_ea9bu64;
        let bytes: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        for width in 1..=32u32 {
            for first_bit in 0..8u32 {
                let packed = super::Packed {
                    bytes: &bytes,
                    bit: u64::from(8 * 100 + first_bit),
                    len: 64,
                    width,
                };
                let lanes = super::simd::Lanes::new(first_bit, width);
                for group in 0..8u32 {
                    let mut scalar = [0; 8];
                    packed.unpack8(group, &mut scalar);
                    let byte = (packed.bit >> 3) as usize + group as usize * width as usize;
                    assert!(byte + super::simd::READ <= bytes.len());
                    let mut simd = [0; 8];
                    // SAFETY: `available()` said the CPU has the instructions, and the assert
                    // keeps the reads inside `bytes`.
                    unsafe {
                        super::simd::unpack8(bytes.as_ptr().add(byte), &lanes, width, &mut simd);
                    }
                    assert_eq!(simd, scalar, "width {width}, first bit {first_bit}, group {group}");
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "a span of settled edges read as words")]
    fn a_settled_span_is_not_words() {
        Edges::new(1, 10).slice(EdgeSpan { start: 3, len: 2 });
    }

    #[test]
    fn racing_tasks_get_disjoint_ranges() {
        let edges = Edges::new(1, 0);
        let ranges: Vec<Vec<(u32, u32)>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8u32)
                .map(|t| {
                    let edges = &edges;
                    scope.spawn(move || {
                        let chunk = Cell::new(Chunk::default());
                        (0..2000u32)
                            .map(|i| {
                                let n = 1 + (i * 7 + t) % 200;
                                (edges.reserve_for_task(&chunk, n), n)
                            })
                            .collect()
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        let mut all: Vec<(u32, u32)> = ranges.into_iter().flatten().collect();
        all.sort_unstable();
        for pair in all.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0);
        }
    }
}
