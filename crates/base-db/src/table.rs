//! The tables [`Files`](crate::Files) reads on every query, without a lock
//! (docs/v2/plan/design/concurrency.md): a read is acquire loads of data that never moves once
//! published, and only writers write.
//!
//! - [`Column`]: a salsa input per dense `u32` ID (a file, a source root), in pages made the
//!   first time an ID in them is set. A read is two acquire loads.
//! - [`Log`]: an append-only list in chunks that never move, as `blaze_ids::Sharded` keeps its
//!   values.
//! - [`Table`]: values found by a hash the caller computed once, in open-addressed tables of
//!   `u32`s per shard, grown by moving to a table twice the size and never rehashed in place
//!   (`blaze_ids::Sharded`'s layout). A lookup is acquire loads and compares; inserting locks
//!   one shard to claim a place, never while making the value.

use std::{
    marker::PhantomData,
    num::NonZero,
    sync::{
        Mutex, OnceLock, PoisonError,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
};

use salsa::plumbing::{AsId, FromId, Id};

const PAGE_BITS: u32 = 12;
const PAGE: usize = 1 << PAGE_BITS;

/// A salsa input per ID up to [`Column::MAX`]. A page holds [`PAGE`] words of the input's ID
/// bits, 0 for none (an input's index is never 0).
///
/// Costs 16 bytes of directory per page (32 KiB a column) and 8 bytes per ID of a page that has
/// one. A read is two acquire loads (the page, then the word) and writes nothing.
pub(crate) struct Column<T> {
    pages: Box<[OnceLock<Box<[AtomicU64; PAGE]>>]>,
    of: PhantomData<fn() -> T>,
}

const _: () = assert!(size_of::<OnceLock<Box<[AtomicU64; PAGE]>>>() <= 16);

impl<T: AsId + FromId + Copy> Column<T> {
    /// The largest file ID the front end takes (`span::EditionedFileId` holds 23 bits of one):
    /// a column indexes `0..=MAX`.
    pub(crate) const MAX: u32 = span::EditionedFileId::MAX_FILE_ID;

    pub(crate) fn new() -> Column<T> {
        let pages = (Self::MAX as usize >> PAGE_BITS) + 1;
        // alloc: the page directory, once per column (32 KiB).
        Column { pages: (0..pages).map(|_| OnceLock::new()).collect(), of: PhantomData }
    }

    #[inline]
    pub(crate) fn get(&self, raw: u32) -> Option<T> {
        let page = self.pages.get((raw >> PAGE_BITS) as usize)?.get()?;
        match page[raw as usize & (PAGE - 1)].load(Ordering::Acquire) {
            0 => None,
            bits => Some(T::from_id(Id::from_bits(bits))),
        }
    }

    /// Sets (or with `None` clears) the input of `raw`. Panics naming it past [`Column::MAX`].
    pub(crate) fn set(&self, raw: u32, value: Option<T>) {
        let Some(page) = self.pages.get((raw >> PAGE_BITS) as usize) else {
            panic!("ID {raw} is past {}, the largest file ID the front end takes", Self::MAX)
        };
        let bits = value.map_or(0, |it| it.as_id().as_bits());
        if bits == 0 && page.get().is_none() {
            return;
        }
        page.get_or_init(new_page)[raw as usize & (PAGE - 1)].store(bits, Ordering::Release);
    }

    /// Every ID that has an input, in order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u32, T)> + '_ {
        self.pages.iter().enumerate().filter_map(|(at, page)| Some((at, page.get()?))).flat_map(
            |(at, page)| {
                page.iter().enumerate().filter_map(move |(slot, word)| {
                    match word.load(Ordering::Acquire) {
                        0 => None,
                        bits => Some((
                            ((at << PAGE_BITS) | slot) as u32,
                            T::from_id(Id::from_bits(bits)),
                        )),
                    }
                })
            },
        )
    }
}

fn new_page() -> Box<[AtomicU64; PAGE]> {
    // alloc: a page, once per 4096 IDs the first time one of them is set.
    let page: Box<[AtomicU64]> = (0..PAGE).map(|_| AtomicU64::new(0)).collect();
    page.try_into().unwrap_or_else(|_| unreachable!("a page of {PAGE} words"))
}

/// Chunk `k` of a [`Log`] holds `FIRST << k` slots, so an index's chunk and place in it are
/// arithmetic and 27 chunks hold every `u32`.
const FIRST: usize = 64;
const CHUNKS: usize = 27;

fn chunk_of(raw: u32) -> (usize, usize) {
    let biased = raw as usize / FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, raw as usize - FIRST * ((1 << chunk) - 1))
}

/// An append-only list whose values never move: `&T` lives as long as the log. Appending
/// claims an index with one `fetch_add`; a read is two acquire loads.
pub(crate) struct Log<T> {
    chunks: [OnceLock<Box<[OnceLock<T>]>>; CHUNKS],
    next: AtomicU32,
}

impl<T> Log<T> {
    pub(crate) fn new() -> Log<T> {
        Log { chunks: std::array::from_fn(|_| OnceLock::new()), next: AtomicU32::new(0) }
    }

    pub(crate) fn push(&self, value: T) -> u32 {
        let raw = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(raw != u32::MAX, "a log of 2^32 values");
        let (chunk, at) = chunk_of(raw);
        let slots =
            self.chunks[chunk].get_or_init(|| (0..FIRST << chunk).map(|_| OnceLock::new()).collect()); // alloc: chunk k once, when the log first reaches it.
        if slots[at].set(value).is_err() {
            unreachable!("index {raw} handed out twice");
        }
        raw
    }

    #[inline]
    pub(crate) fn get(&self, raw: u32) -> Option<&T> {
        let (chunk, at) = chunk_of(raw);
        self.chunks[chunk].get()?.get(at)?.get()
    }

    /// The values appended, in order; one still being appended is left out.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        (0..self.next.load(Ordering::Acquire)).filter_map(move |raw| self.get(raw))
    }
}

/// Table `k` of a shard has `TABLE << k` places: 28 hold more than a `u32` of entries.
const TABLE: usize = 16;
const TABLES: usize = 28;

/// Values found by a 64-bit hash of their key (computed once by the caller), each kept once.
///
/// 4 bytes a place, at most half full, plus the value and its hash in a [`Log`]. A lookup is
/// one shard pick, acquire loads along the probe and a key compare per entry whose hash
/// matches; nothing is written.
pub(crate) struct Table<T> {
    values: Log<(u64, T)>,
    /// Made on its first insert: a table costs 16 bytes a shard until then.
    shards: Box<[OnceLock<Box<Shard>>]>,
    /// A hash's shard is its top bits.
    shift: u32,
}

struct Shard {
    /// Held to insert; holds how many values the shard has.
    lock: Mutex<u32>,
    /// Which of `tables` is the one in use.
    level: AtomicUsize,
    /// A place holds an index into `values` plus one; 0 is empty.
    tables: [OnceLock<Box<[AtomicU32]>>; TABLES],
}

/// Four times the cores, a power of two (concurrency.md: "Writers shard by hash").
fn shard_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(1, NonZero::get);
        (cores * 4).next_power_of_two().max(4)
    })
}

impl<T> Table<T> {
    pub(crate) fn new() -> Table<T> {
        let shards = shard_count();
        Table {
            values: Log::new(),
            // alloc: once per table: 16 bytes a shard.
            shards: (0..shards).map(|_| OnceLock::new()).collect(),
            shift: u64::BITS - shards.trailing_zeros(),
        }
    }

    fn shard(&self, hash: u64) -> &OnceLock<Box<Shard>> {
        &self.shards[(hash >> self.shift) as usize]
    }

    /// Where `hash`'s probe starts: its low bits, folded with its high ones.
    fn home(hash: u64) -> usize {
        (hash ^ (hash >> 32)) as usize
    }

    fn table(shard: &Shard, level: usize) -> &[AtomicU32] {
        // alloc: a shard's table at each size it grows to, once.
        shard.tables[level].get_or_init(|| (0..TABLE << level).map(|_| AtomicU32::new(0)).collect())
    }

    fn probe(&self, table: &[AtomicU32], hash: u64, eq: &impl Fn(&T) -> bool) -> Option<u32> {
        let mask = table.len() - 1;
        let mut at = Self::home(hash) & mask;
        loop {
            match table[at].load(Ordering::Acquire) {
                0 => return None,
                plus_one => {
                    let (stored, value) =
                        self.values.get(plus_one - 1).expect("a table entry before its value");
                    if *stored == hash && eq(value) {
                        return Some(plus_one - 1);
                    }
                }
            }
            at = (at + 1) & mask;
        }
    }

    fn vacant(table: &[AtomicU32], hash: u64) -> usize {
        let mask = table.len() - 1;
        let mut at = Self::home(hash) & mask;
        while table[at].load(Ordering::Relaxed) != 0 {
            at = (at + 1) & mask;
        }
        at
    }

    /// The index of the value whose key `eq` matches, `hash` being its key's hash.
    #[inline]
    pub(crate) fn find(&self, hash: u64, eq: impl Fn(&T) -> bool) -> Option<u32> {
        let shard = self.shard(hash).get()?;
        let level = shard.level.load(Ordering::Acquire);
        self.probe(shard.tables[level].get()?, hash, &eq)
    }

    /// The index of the value whose key `eq` matches, inserting `make()` if there is none, and
    /// whether it did. `make` runs with the shard locked: it must not insert into this table,
    /// call into the database or block.
    pub(crate) fn insert(
        &self,
        hash: u64,
        eq: impl Fn(&T) -> bool,
        make: impl FnOnce() -> T,
    ) -> (u32, bool) {
        if let Some(found) = self.find(hash, &eq) {
            return (found, false);
        }
        let shard = self.shard(hash).get_or_init(|| {
            // alloc: a shard, on its first insert.
            Box::new(Shard {
                lock: Mutex::new(0),
                level: AtomicUsize::new(0),
                tables: std::array::from_fn(|_| OnceLock::new()),
            })
        });
        let mut count = shard.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut level = shard.level.load(Ordering::Acquire);
        if let Some(found) = self.probe(Self::table(shard, level), hash, &eq) {
            return (found, false);
        }
        if (*count as usize + 1) * 2 > TABLE << level {
            level = self.grow(shard, level);
        }
        let raw = self.values.push((hash, make()));
        let table = Self::table(shard, level);
        table[Self::vacant(table, hash)].store(raw + 1, Ordering::Release);
        *count += 1;
        (raw, true)
    }

    /// Moves a full shard's entries into a table twice its size and makes it the one in use.
    /// Called with the shard locked; the old table stays, as a reader may be probing it.
    fn grow(&self, shard: &Shard, level: usize) -> usize {
        assert!(level + 1 < TABLES, "a shard of more than 2^30 values");
        let old = Self::table(shard, level);
        let new = Self::table(shard, level + 1);
        for entry in old {
            let plus_one = entry.load(Ordering::Relaxed);
            if plus_one == 0 {
                continue;
            }
            let (hash, _) = self.values.get(plus_one - 1).expect("a table entry before its value");
            new[Self::vacant(new, *hash)].store(plus_one, Ordering::Relaxed);
        }
        shard.level.store(level + 1, Ordering::Release);
        level + 1
    }

    #[inline]
    pub(crate) fn get(&self, raw: u32) -> &T {
        &self.values.get(raw).expect("an index this table did not hand out").1
    }

    /// Every value, in the order they were inserted (which threads decide: sort what leaves).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.values.iter().map(|(_, value)| value)
    }
}

/// The hash a [`Table`] is keyed by: `FxHash`, as every key is the build's own (paths and names
/// its sources wrote), never an attacker's.
#[inline]
pub(crate) fn hash_of(key: impl std::hash::Hash) -> u64 {
    use std::hash::BuildHasher;
    rustc_hash::FxBuildHasher.hash_one(key)
}
