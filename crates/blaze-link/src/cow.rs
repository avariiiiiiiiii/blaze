//! Copy-on-write storage for an image's state, so that a stage's copy costs a pointer per
//! chunk and copies only what it writes: [`CowVec`] (a column), [`CowList`] (many short lists
//! in shared chunks, CSR), [`CowBytes`] (a region's bytes, by page); and the dense sets a stage
//! keeps ([`IdSet`], [`SymSet`]).

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

use crate::model::Sym;
use crate::symbols::Symbols;

const CHUNK: usize = 1024;

/// A column cloned in O(len / 1024): chunks are shared until written.
#[derive(Debug)]
pub(crate) struct CowVec<T> {
    chunks: Vec<Arc<Vec<T>>>,
    len: usize,
}

impl<T> Clone for CowVec<T> {
    fn clone(&self) -> Self {
        CowVec { chunks: self.chunks.clone(), len: self.len }
    }
}

impl<T> Default for CowVec<T> {
    fn default() -> Self {
        CowVec { chunks: Vec::new(), len: 0 }
    }
}

impl<T: Clone> CowVec<T> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, at: usize) -> Option<&T> {
        if at >= self.len {
            return None;
        }
        Some(&self.chunks[at / CHUNK][at % CHUNK])
    }

    #[inline]
    pub fn get_mut(&mut self, at: usize) -> Option<&mut T> {
        if at >= self.len {
            return None;
        }
        Some(&mut Arc::make_mut(&mut self.chunks[at / CHUNK])[at % CHUNK])
    }

    pub fn push(&mut self, value: T) -> usize {
        let at = self.len;
        if at % CHUNK == 0 {
            self.chunks.push(Arc::new(Vec::with_capacity(CHUNK)));
        }
        Arc::make_mut(self.chunks.last_mut().expect("a chunk was just made")).push(value);
        self.len += 1;
        at
    }

    /// Sets `at`, growing the column with `fill` to reach it.
    pub fn set(&mut self, at: usize, value: T, fill: impl Fn() -> T) {
        while self.len <= at {
            self.push(fill());
        }
        *self.get_mut(at).expect("grown to it") = value;
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.chunks.iter().flat_map(|chunk| chunk.iter())
    }
}

/// A list in a [`CowList`]: which chunk, where in it, how long.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct ListRef {
    pub chunk: u32,
    pub start: u32,
    pub len: u32,
}

const _: () = assert!(size_of::<ListRef>() == 12);

impl ListRef {
    pub const EMPTY: ListRef = ListRef { chunk: 0, start: 0, len: 0 };

    pub fn len(self) -> usize {
        self.len as usize
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Many lists in shared chunks (CSR): every piece's fixups in one, every symbol's referrers in
/// another. A list never straddles chunks, so it reads as one slice. Pushing appends to the
/// last chunk (copying it, at most 64 KiB, the first time a stage writes it) or starts a new
/// one; a list longer than a chunk gets one of its own. Replaced lists stay as garbage until
/// the state is compacted.
#[derive(Debug)]
pub(crate) struct CowList<T> {
    chunks: Vec<Arc<Vec<T>>>,
    live: usize,
    garbage: usize,
}

impl<T> Clone for CowList<T> {
    fn clone(&self) -> Self {
        CowList { chunks: self.chunks.clone(), live: self.live, garbage: self.garbage }
    }
}

impl<T> Default for CowList<T> {
    fn default() -> Self {
        CowList { chunks: Vec::new(), live: 0, garbage: 0 }
    }
}

impl<T: Copy> CowList<T> {
    /// Elements per chunk: 64 KiB of them.
    const CHUNK: usize = {
        let n = 64 * 1024 / if size_of::<T>() == 0 { 1 } else { size_of::<T>() };
        if n < 16 { 16 } else { n }
    };

    #[inline]
    pub fn get(&self, list: ListRef) -> &[T] {
        if list.len == 0 {
            return &[];
        }
        let start = list.start as usize;
        &self.chunks[list.chunk as usize][start..start + list.len as usize]
    }

    /// The list's elements to write in place; its chunk is copied first when shared.
    pub fn get_mut(&mut self, list: ListRef) -> &mut [T] {
        if list.len == 0 {
            return &mut [];
        }
        let start = list.start as usize;
        &mut Arc::make_mut(&mut self.chunks[list.chunk as usize])[start..start + list.len as usize]
    }

    pub fn push(&mut self, items: &[T]) -> ListRef {
        if items.is_empty() {
            return ListRef::EMPTY;
        }
        let list = self.room(items.len());
        let chunk = Arc::make_mut(&mut self.chunks[list.chunk as usize]);
        chunk.extend_from_slice(items);
        self.live += items.len();
        list
    }

    /// A list of `len` copies of `fill`.
    pub fn push_filled(&mut self, len: usize, fill: T) -> ListRef {
        if len == 0 {
            return ListRef::EMPTY;
        }
        let list = self.room(len);
        let chunk = Arc::make_mut(&mut self.chunks[list.chunk as usize]);
        chunk.resize(chunk.len() + len, fill);
        self.live += len;
        list
    }

    /// Where a list of `len` goes: the end of the last chunk when it fits, else a new chunk.
    fn room(&mut self, len: usize) -> ListRef {
        let fits = self.chunks.last().is_some_and(|it| it.len() + len <= Self::CHUNK);
        if !fits {
            self.chunks.push(Arc::new(Vec::with_capacity(len.max(Self::CHUNK))));
        }
        let chunk = self.chunks.len() - 1;
        let start = self.chunks[chunk].len();
        assert!(chunk <= u32::MAX as usize && len <= u32::MAX as usize, "a list arena past 2^32");
        ListRef { chunk: chunk as u32, start: start as u32, len: len as u32 }
    }

    /// Counts a list that nothing refers to any more as garbage.
    pub fn free(&mut self, list: ListRef) {
        self.live -= list.len();
        self.garbage += list.len();
    }

    pub fn garbage(&self) -> usize {
        self.garbage
    }

    pub fn live(&self) -> usize {
        self.live
    }
}

/// A set of dense `u32` ids (pieces, slots): a bitset answers membership, and the members are
/// kept in the order they were first added, so iterating costs the members, not the range.
#[derive(Clone, Default, Debug)]
pub(crate) struct IdSet {
    bits: Vec<u64>,
    listed: Vec<u64>,
    list: Vec<u32>,
    len: usize,
}

impl IdSet {
    #[inline]
    pub fn contains(&self, id: u32) -> bool {
        let (word, bit) = (id as usize / 64, id % 64);
        self.bits.get(word).is_some_and(|it| it & (1 << bit) != 0)
    }

    pub fn insert(&mut self, id: u32) -> bool {
        let (word, bit) = (id as usize / 64, id % 64);
        if self.bits.len() <= word {
            self.bits.resize(word + 1, 0);
            self.listed.resize(word + 1, 0);
        }
        if self.bits[word] & (1 << bit) != 0 {
            return false;
        }
        self.bits[word] |= 1 << bit;
        self.len += 1;
        if self.listed[word] & (1 << bit) == 0 {
            self.listed[word] |= 1 << bit;
            self.list.push(id);
        }
        true
    }

    pub fn remove(&mut self, id: u32) -> bool {
        let (word, bit) = (id as usize / 64, id % 64);
        match self.bits.get_mut(word) {
            Some(it) if *it & (1 << bit) != 0 => {
                *it &= !(1 << bit);
                self.len -= 1;
                true
            }
            _ => false,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The members, each once, in the order they were first added.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.list.iter().copied().filter(|it| self.contains(*it))
    }

    pub fn sorted(&self) -> Vec<u32> {
        let mut out: Vec<u32> = self.iter().collect();
        out.sort_unstable();
        out
    }

    pub fn extend(&mut self, ids: impl IntoIterator<Item = u32>) {
        for id in ids {
            self.insert(id);
        }
    }
}

/// A set of symbols: an [`IdSet`] per symbol shard.
#[derive(Clone, Debug)]
pub(crate) struct SymSet {
    shards: Vec<IdSet>,
    len: usize,
}

impl Default for SymSet {
    fn default() -> Self {
        SymSet { shards: vec![IdSet::default(); Symbols::SHARDS], len: 0 }
    }
}

impl SymSet {
    pub fn insert(&mut self, sym: Sym) -> bool {
        let (shard, at) = Symbols::split(sym);
        let new = self.shards[shard].insert(at as u32);
        self.len += usize::from(new);
        new
    }

    pub fn remove(&mut self, sym: Sym) -> bool {
        let (shard, at) = Symbols::split(sym);
        let gone = self.shards[shard].remove(at as u32);
        self.len -= usize::from(gone);
        gone
    }

    pub fn contains(&self, sym: Sym) -> bool {
        let (shard, at) = Symbols::split(sym);
        self.shards[shard].contains(at as u32)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The members, shard by shard, each in the order it was first added.
    pub fn iter(&self) -> impl Iterator<Item = Sym> + '_ {
        self.shards
            .iter()
            .enumerate()
            .flat_map(|(shard, it)| it.iter().map(move |at| Symbols::join(shard, at as usize)))
    }

    pub fn extend(&mut self, syms: impl IntoIterator<Item = Sym>) {
        for sym in syms {
            self.insert(sym);
        }
    }
}

const PAGE: usize = 16 * 1024;

type Page = [u8; PAGE];

/// A region's bytes: the buffer the last full link wrote, and the 16 KiB pages written since,
/// each shared with the states it came from until written again. A stage that writes one
/// function copies the page it is in, not the region; reading a range that lies in the
/// unwritten buffer or in one page borrows it.
#[derive(Clone, Debug, Default)]
pub(crate) struct CowBytes {
    base: Arc<Vec<u8>>,
    /// How much of `base` is still the bytes there: a truncation shortens it, so growing
    /// again reads zeros.
    base_len: usize,
    /// One per page of `0..len`: the page as written, or `None` for `base` (zeros past
    /// `base_len`).
    pages: Vec<Option<Arc<Page>>>,
    len: usize,
}

impl CowBytes {
    pub const PAGE: usize = PAGE;

    pub fn from_vec(bytes: Vec<u8>) -> CowBytes {
        let len = bytes.len();
        CowBytes { base: Arc::new(bytes), base_len: len, pages: vec![None; len.div_ceil(PAGE)], len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `at..at + len`, borrowed when it is in the unwritten buffer or in one written page.
    pub fn read(&self, at: usize, len: usize) -> Cow<'_, [u8]> {
        if let Some(it) = self.slice(at, len) {
            return Cow::Borrowed(it);
        }
        let mut out = vec![0; len];
        self.copy_to(at, &mut out);
        Cow::Owned(out)
    }

    /// `at..at + len` if it can be borrowed (see [`CowBytes::read`]).
    pub fn slice(&self, at: usize, len: usize) -> Option<&[u8]> {
        let end = at.checked_add(len).filter(|it| *it <= self.len).expect("a read past the bytes' end");
        if len == 0 {
            return Some(&[]);
        }
        let (first, last) = (at / PAGE, (end - 1) / PAGE);
        if end <= self.base_len && self.pages[first..=last].iter().all(Option::is_none) {
            return Some(&self.base[at..end]);
        }
        match &self.pages[first] {
            Some(page) if first == last => Some(&page[at % PAGE..at % PAGE + len]),
            _ => None,
        }
    }

    pub fn copy_to(&self, mut at: usize, mut out: &mut [u8]) {
        assert!(at + out.len() <= self.len, "a read past the bytes' end");
        while !out.is_empty() {
            let (page, from) = (at / PAGE, at % PAGE);
            let n = (PAGE - from).min(out.len());
            let (head, rest) = out.split_at_mut(n);
            match &self.pages[page] {
                Some(it) => head.copy_from_slice(&it[from..from + n]),
                None => {
                    let have = self.base_len.saturating_sub(at).min(n);
                    head[..have].copy_from_slice(&self.base[at..at + have]);
                    head[have..].fill(0);
                }
            }
            out = rest;
            at += n;
        }
    }

    /// Page `index` to write, copied from the buffer (or made) the first time.
    fn page_mut(&mut self, index: usize) -> &mut Page {
        let slot = &mut self.pages[index];
        if slot.is_none() {
            let mut page: Arc<Page> = Arc::new([0; PAGE]);
            let start = index * PAGE;
            let have = self.base_len.saturating_sub(start).min(PAGE);
            Arc::get_mut(&mut page).expect("just made")[..have].copy_from_slice(&self.base[start..start + have]);
            *slot = Some(page);
        }
        Arc::make_mut(slot.as_mut().expect("made above"))
    }

    pub fn write(&mut self, at: usize, bytes: &[u8]) {
        if at + bytes.len() > self.len {
            self.resize(at + bytes.len());
        }
        let (mut at, mut bytes) = (at, bytes);
        while !bytes.is_empty() {
            let (page, from) = (at / PAGE, at % PAGE);
            let n = (PAGE - from).min(bytes.len());
            self.page_mut(page)[from..from + n].copy_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            at += n;
        }
    }

    pub fn fill(&mut self, at: usize, len: usize, byte: u8) {
        if at + len > self.len {
            self.resize(at + len);
        }
        let (mut at, end) = (at, at + len);
        while at < end {
            let (page, from) = (at / PAGE, at % PAGE);
            let n = (PAGE - from).min(end - at);
            self.page_mut(page)[from..from + n].fill(byte);
            at += n;
        }
    }

    /// Grows with zeros or truncates.
    pub fn resize(&mut self, len: usize) {
        if len < self.len {
            self.base_len = self.base_len.min(len);
            self.pages.truncate(len.div_ceil(PAGE));
            if len % PAGE != 0 {
                let last = len / PAGE;
                if self.pages[last].is_some() {
                    self.page_mut(last)[len % PAGE..].fill(0);
                }
            }
        }
        self.len = len;
        self.pages.resize(len.div_ceil(PAGE), None);
    }

    /// All the bytes: borrowed when no page was written since the buffer.
    pub fn contiguous(&self) -> Cow<'_, [u8]> {
        if self.len <= self.base_len && self.pages.iter().all(Option::is_none) {
            return Cow::Borrowed(&self.base[..self.len]);
        }
        let mut out = vec![0; self.len];
        self.copy_to(0, &mut out);
        Cow::Owned(out)
    }

    /// The page-aligned ranges of `self` that may differ from `old`: pages both share are
    /// skipped without reading them.
    pub fn changed_since(&self, old: &CowBytes) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = Vec::new();
        let same_base = Arc::ptr_eq(&self.base, &old.base);
        for (index, page) in self.pages.iter().enumerate() {
            let (start, end) = (index * PAGE, ((index + 1) * PAGE).min(self.len));
            let same = match (page, old.pages.get(index)) {
                (Some(a), Some(Some(b))) => Arc::ptr_eq(a, b),
                (None, Some(None)) => {
                    let base = |it: &CowBytes| it.base_len.clamp(start, end);
                    same_base && base(self) == base(old) && end <= old.len
                }
                _ => false,
            };
            if same {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.end == start => last.end = end,
                _ => out.push(start..end),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_shares_until_written() {
        let mut a = CowVec::default();
        for n in 0..3000u32 {
            a.push(n);
        }
        let mut b = a.clone();
        *b.get_mut(5).unwrap() = 99;
        b.set(4000, 7, || 0);
        assert_eq!(a.get(5), Some(&5));
        assert_eq!(b.get(5), Some(&99));
        assert_eq!(a.len(), 3000);
        assert_eq!(b.len(), 4001);
        assert_eq!(b.get(3500), Some(&0));
        assert!(Arc::ptr_eq(&a.chunks[1], &b.chunks[1]));
        assert!(!Arc::ptr_eq(&a.chunks[0], &b.chunks[0]));
        assert_eq!(a.iter().copied().sum::<u32>(), (0..3000).sum());
    }

    #[test]
    fn lists_are_slices_and_a_clone_copies_only_the_chunk_it_writes() {
        let mut a: CowList<u64> = CowList::default();
        let lists: Vec<(ListRef, Vec<u64>)> = (0..5000u64)
            .map(|n| {
                let items: Vec<u64> = (0..n % 7).map(|it| n * 10 + it).collect();
                (a.push(&items), items)
            })
            .collect();
        for (list, items) in &lists {
            assert_eq!(a.get(*list), &items[..]);
        }
        let big: Vec<u64> = (0..20_000).collect();
        let big_list = a.push(&big);
        assert_eq!(a.get(big_list), &big[..]);
        let mut b = a.clone();
        let (first, _) = lists.iter().find(|it| !it.0.is_empty()).expect("a non-empty list");
        b.get_mut(*first)[0] = u64::MAX;
        let added = b.push(&[1, 2, 3]);
        assert_eq!(b.get(added), &[1, 2, 3]);
        assert_eq!(b.get(*first)[0], u64::MAX);
        assert_ne!(a.get(*first)[0], u64::MAX);
        for (at, (x, y)) in a.chunks.iter().zip(&b.chunks).enumerate() {
            assert_eq!(Arc::ptr_eq(x, y), at != first.chunk as usize);
        }
        b.free(*first);
        assert_eq!(b.garbage(), first.len());
        assert_eq!(a.garbage(), 0);
    }

    #[test]
    fn id_sets_iterate_members_once_in_order() {
        let mut set = IdSet::default();
        for id in [5, 900, 3, 5, 64, 0] {
            set.insert(id);
        }
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![5, 900, 3, 64, 0]);
        assert!(set.remove(900));
        assert!(!set.remove(900));
        set.insert(900);
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![5, 900, 3, 64, 0]);
        assert_eq!(set.sorted(), vec![0, 3, 5, 64, 900]);
        assert_eq!(set.len(), 5);
        assert!(!set.contains(1_000_000));
        let mut syms = SymSet::default();
        let a = Symbols::join(3, 7);
        assert!(syms.insert(a));
        assert!(!syms.insert(a));
        assert!(syms.contains(a));
        assert_eq!(syms.iter().collect::<Vec<_>>(), vec![a]);
        assert!(syms.remove(a));
        assert!(syms.is_empty());
    }

    /// The pages against a plain vector given the same writes.
    #[test]
    fn paged_bytes_read_as_a_vector_would() {
        let start: Vec<u8> = (0..(3 * PAGE + 100)).map(|it| it as u8).collect();
        let a = CowBytes::from_vec(start.clone());
        let mut b = a.clone();
        let mut reference = start.clone();
        let mut write = |b: &mut CowBytes, at: usize, bytes: &[u8]| {
            b.write(at, bytes);
            if reference.len() < at + bytes.len() {
                reference.resize(at + bytes.len(), 0);
            }
            reference[at..at + bytes.len()].copy_from_slice(bytes);
        };
        write(&mut b, PAGE - 3, &[1, 2, 3, 4, 5, 6]);
        write(&mut b, 3 * PAGE + 90, &[9; 40]);
        assert!(matches!(b.read(10, 20), Cow::Borrowed(_)));
        assert!(matches!(b.read(PAGE + 1, 20), Cow::Borrowed(_)));
        assert_eq!(&*b.read(PAGE - 5, 10), &reference[PAGE - 5..PAGE + 5]);
        assert_eq!(&*b.contiguous(), &reference[..]);
        assert_eq!(&*a.contiguous(), &start[..]);
        assert!(matches!(a.contiguous(), Cow::Borrowed(_)));
        assert_eq!(b.changed_since(&a), vec![0..2 * PAGE, 3 * PAGE..b.len()]);
        b.resize(PAGE + 10);
        reference.truncate(PAGE + 10);
        b.resize(2 * PAGE);
        reference.resize(2 * PAGE, 0);
        assert_eq!(&*b.contiguous(), &reference[..]);
        b.fill(5, 3, 0xee);
        reference[5..8].fill(0xee);
        assert_eq!(&*b.read(0, 16), &reference[..16]);
    }
}
