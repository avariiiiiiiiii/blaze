//! blaze v2's IDs (docs/v2/plan/lanes/format.md section 5, "IDs, two kinds").
//!
//! - A [`ContentId`] is the BLAKE3 hash of what a thing is, under a domain that names the
//!   kind of thing and its schema. It is the identity used in every hash, key and output.
//! - An [`Idx`] is a dense `u32` index into one arena, assigned in arrival order by parallel
//!   workers: it differs between runs, so it never goes into a hash, a key or the bytes of an
//!   output. It may be kept in the daemon's image, which is one run's state.
//! - An [`Interner`] holds one arena and maps each way between the two; [`Sharded`] is the
//!   one blaze uses.
//! - A [`Fingerprint`] is the 128 bits of a result kept for early cutoff (docs/v2/plan/lanes/items.md
//!   section 6.2).

use std::{
    fmt, hash,
    marker::PhantomData,
    sync::{
        Mutex, OnceLock, PoisonError,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
};

use blake3::hazmat::{self, ContextKey, HasherExt};
use serde::{Deserialize, Serialize};

/// The BLAKE3 hash of what something is. Two things with one content ID are the same thing.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[repr(transparent)]
pub struct ContentId(pub [u8; 32]);

impl ContentId {
    /// The content ID of `bytes` as a thing of `domain`. A domain names what the bytes are
    /// and the version of their schema (`"blaze v2 file 1"`): equal bytes of two domains have
    /// two IDs.
    pub fn of(domain: &'static str, bytes: &[u8]) -> ContentId {
        let mut hasher = Hasher::new(domain);
        hasher.update(bytes);
        hasher.finish()
    }

    /// [`ContentId::of`] for a [`Domain`], its context key read with one acquire load.
    pub fn of_domain(domain: &Domain, bytes: &[u8]) -> ContentId {
        let mut hasher = Hasher::of(domain);
        hasher.update(bytes);
        hasher.finish()
    }

    /// `inputs.iter().map(|it| ContentId::of(domain, it))`, the domain's context key looked up
    /// once for all of them.
    pub fn of_many(domain: &'static str, inputs: &[&[u8]]) -> Vec<ContentId> {
        let key = context_key(domain);
        inputs
            .iter()
            .map(|input| {
                let mut hasher = blake3::Hasher::new_from_context_key(key);
                hasher.update(input);
                ContentId(*hasher.finalize().as_bytes())
            })
            .collect()
    }

    /// The first 128 bits, as a result's fingerprint.
    pub fn fingerprint(&self) -> Fingerprint {
        let mut low = [0; 16];
        low.copy_from_slice(&self.0[..16]);
        Fingerprint(u128::from_le_bytes(low))
    }
}

impl fmt::Debug for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for ContentId {
    /// The first 12 hex digits, as git shows a short id.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0[..6].iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::LowerHex for ContentId {
    /// All 64 hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// Builds a [`ContentId`] from parts. It takes bytes and other content IDs only, so a dense
/// [`Idx`] can't be hashed but by spelling out [`Idx::raw`], which a reviewer sees.
pub struct Hasher(blake3::Hasher);

/// A hash domain whose BLAKE3 context key is derived once, on first use: what a domain hashed
/// millions of times is declared as (`pub static KEY: Domain = Domain::new("blaze v2 key 1")`).
pub struct Domain {
    name: &'static str,
    key: OnceLock<ContextKey>,
}

impl Domain {
    pub const fn new(name: &'static str) -> Domain {
        Domain { name, key: OnceLock::new() }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    fn key(&self) -> &ContextKey {
        self.key.get_or_init(|| hazmat::hash_derive_key_context(self.name))
    }
}

/// How many distinct domain names [`Hasher::new`] keeps the context keys of.
const DOMAIN_SLOTS: usize = 1024;

/// The context key of each domain name hashed so far, open-addressed by the name's text: a
/// slot once set never changes, so a lookup is acquire loads and compares.
static DOMAIN_KEYS: [OnceLock<(&'static str, ContextKey)>; DOMAIN_SLOTS] =
    [const { OnceLock::new() }; DOMAIN_SLOTS];

/// FNV-1a: domain names are a few dozen bytes, hashed once per [`Hasher::new`].
fn name_hash(name: &str) -> usize {
    name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3)
    }) as usize
}

/// `blake3::hazmat::hash_derive_key_context(domain)`, derived once per name.
fn context_key(domain: &'static str) -> &'static ContextKey {
    let mut at = name_hash(domain) & (DOMAIN_SLOTS - 1);
    for _ in 0..DOMAIN_SLOTS {
        let slot = &DOMAIN_KEYS[at];
        let (name, key) = match slot.get() {
            Some(entry) => entry,
            None => slot.get_or_init(|| (domain, hazmat::hash_derive_key_context(domain))),
        };
        if *name == domain {
            return key;
        }
        at = (at + 1) & (DOMAIN_SLOTS - 1);
    }
    panic!("more than {DOMAIN_SLOTS} hash domains: raise blaze-ids' DOMAIN_SLOTS");
}

impl Hasher {
    pub fn new(domain: &'static str) -> Hasher {
        Hasher(blake3::Hasher::new_from_context_key(context_key(domain)))
    }

    pub fn of(domain: &Domain) -> Hasher {
        Hasher(blake3::Hasher::new_from_context_key(domain.key()))
    }

    pub fn update(&mut self, bytes: &[u8]) -> &mut Hasher {
        self.0.update(bytes);
        self
    }

    /// A length-prefixed part, so `("ab", "c")` and `("a", "bc")` hash apart.
    pub fn part(&mut self, bytes: &[u8]) -> &mut Hasher {
        self.0.update(&(bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }

    pub fn id(&mut self, id: &ContentId) -> &mut Hasher {
        self.0.update(&id.0);
        self
    }

    pub fn finish(&self) -> ContentId {
        ContentId(*self.0.finalize().as_bytes())
    }
}

/// 128 bits that stand for a result's value: a rerun that produces the same fingerprint stops
/// there (early cutoff). Kept with every dependency edge and when the value is evicted.
/// Serialized as 16 little-endian bytes: CBOR has no integer this wide.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[repr(transparent)]
pub struct Fingerprint(pub u128);

impl Serialize for Fingerprint {
    fn serialize<S: serde::Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        self.0.to_le_bytes().serialize(to)
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: serde::Deserializer<'de>>(from: D) -> Result<Fingerprint, D::Error> {
        <[u8; 16]>::deserialize(from).map(|bytes| Fingerprint(u128::from_le_bytes(bytes)))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}", (self.0 >> 96) as u32)
    }
}

/// A dense index into the arena of `T`s that made it. Only meaningful for that arena.
#[repr(transparent)]
pub struct Idx<T> {
    raw: u32,
    of: PhantomData<fn() -> T>,
}

impl<T> Idx<T> {
    pub const fn from_raw(raw: u32) -> Idx<T> {
        Idx { raw, of: PhantomData }
    }

    pub const fn raw(self) -> u32 {
        self.raw
    }
}

impl<T> Clone for Idx<T> {
    fn clone(&self) -> Idx<T> {
        *self
    }
}

impl<T> Copy for Idx<T> {}

impl<T> PartialEq for Idx<T> {
    fn eq(&self, other: &Idx<T>) -> bool {
        self.raw == other.raw
    }
}

impl<T> Eq for Idx<T> {}

impl<T> PartialOrd for Idx<T> {
    fn partial_cmp(&self, other: &Idx<T>) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Idx<T> {
    fn cmp(&self, other: &Idx<T>) -> std::cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl<T> hash::Hash for Idx<T> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<T> fmt::Debug for Idx<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.raw)
    }
}

/// One arena of `T`s, append-only and sharded by content ID: interning from many workers
/// takes no global lock, and a reader never locks. A value once interned never moves or
/// changes, so `&T` lives as long as the interner.
pub trait Interner<T>: Sync {
    /// The index of the value whose content ID is `id`, interning `value` if it has none.
    /// `value` must be what `id` is the content ID of. It may run and its value be dropped
    /// when another worker interns `id` at the same time.
    fn intern(&self, id: ContentId, value: impl FnOnce() -> T) -> Idx<T>;
    fn get(&self, idx: Idx<T>) -> &T;
    fn content(&self, idx: Idx<T>) -> ContentId;
    fn find(&self, id: &ContentId) -> Option<Idx<T>>;
    fn len(&self) -> usize;
}

/// The [`Interner`] of blaze v2: values in an append-only arena of chunks that never move,
/// found by content ID in one of [`shard_count`] open-addressed tables.
///
/// [`Interner::get`], [`Interner::content`] and [`Interner::find`] take no lock. Interning a
/// new value computes it first, then locks the one shard its content ID falls in only to look
/// again and insert (a racing loser's value is dropped). A shard's table grows by copying into
/// one twice its size, published once full; the tables it outgrew are kept until the interner
/// is dropped, as a reader may be probing one, so the tables cost at most twice the last one.
///
/// A shard hands out indices from a block of [`BLOCK`] it claims at a time, so writers to
/// different shards share no counter. Indices are dense but for the unused rest of each
/// shard's last block: every index is below [`Sharded::end`], at most `BLOCK - 1` per shard
/// past [`Interner::len`].
pub struct Sharded<T> {
    slots: Chunks<T>,
    /// The first index no shard has claimed.
    next: AtomicU64,
    shards: Box<[Shard]>,
    /// `shards.len() - 1`.
    mask: usize,
}

/// How many indices a shard claims at a time.
pub const BLOCK: u32 = 64;

/// The shards of every [`Sharded`]: a power of two at least 4 times the cores, at least 4, so
/// writers rarely meet on one shard and each shard's table, copied under its lock to grow,
/// stays small.
pub fn shard_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(1, |it| it.get());
        (cores * 4).next_power_of_two().max(4)
    })
}

/// Chunk `k` holds `FIRST << k` slots, so an index's chunk and place in it are arithmetic and
/// 27 chunks hold every `u32`.
const FIRST: usize = 64;
const CHUNKS: usize = 27;
/// Table `k` of a shard has `TABLE << k` places: 28 hold more than a `u32` of entries.
const TABLE: usize = 16;
const TABLES: usize = 28;

type Chunks<T> = [OnceLock<Box<[OnceLock<(ContentId, T)>]>>; CHUNKS];

/// The indices `next..end` a shard has claimed and not yet handed out.
struct Claimed {
    next: u32,
    end: u32,
}

/// One cache line pair per shard, so writers to neighbouring shards don't share a line.
#[repr(align(128))]
struct Shard {
    /// Held only to look again and insert.
    lock: Mutex<Claimed>,
    /// How many values the shard has, written under `lock`.
    len: AtomicU32,
    /// Which of `tables` is the one in use.
    level: AtomicUsize,
    /// A place holds an index plus one; 0 is empty.
    tables: [OnceLock<Box<[AtomicU32]>>; TABLES],
}

fn chunk_of(raw: u32) -> (usize, usize) {
    let biased = raw as usize / FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, raw as usize - FIRST * ((1 << chunk) - 1))
}

impl<T: Send + Sync> Sharded<T> {
    pub fn new() -> Sharded<T> {
        let count = shard_count();
        Sharded {
            slots: std::array::from_fn(|_| OnceLock::new()),
            next: AtomicU64::new(0),
            shards: (0..count)
                .map(|_| Shard {
                    lock: Mutex::new(Claimed { next: 0, end: 0 }),
                    len: AtomicU32::new(0),
                    level: AtomicUsize::new(0),
                    tables: std::array::from_fn(|_| OnceLock::new()),
                })
                .collect(),
            mask: count - 1,
        }
    }

    /// Above every index handed out so far, by at most [`BLOCK`] per shard: what a column
    /// indexed by [`Idx`] is sized to.
    pub fn end(&self) -> usize {
        self.next.load(Ordering::Acquire) as usize
    }

    fn slot(&self, raw: u32) -> Option<&(ContentId, T)> {
        let (chunk, at) = chunk_of(raw);
        self.slots[chunk].get()?.get(at)?.get()
    }

    fn shard(&self, id: &ContentId) -> &Shard {
        let mut word = [0; 4];
        word.copy_from_slice(&id.0[..4]);
        &self.shards[u32::from_le_bytes(word) as usize & self.mask]
    }

    /// Where `id`'s probe starts: bits of the ID the shard was not picked by.
    fn home(id: &ContentId) -> usize {
        let mut word = [0; 8];
        word.copy_from_slice(&id.0[8..16]);
        u64::from_le_bytes(word) as usize
    }

    fn table(shard: &Shard, level: usize) -> &[AtomicU32] {
        shard.tables[level].get_or_init(|| (0..TABLE << level).map(|_| AtomicU32::new(0)).collect())
    }

    fn probe(&self, table: &[AtomicU32], id: &ContentId) -> Result<Idx<T>, usize> {
        let mask = table.len() - 1;
        let mut at = Self::home(id) & mask;
        loop {
            match table[at].load(Ordering::Acquire) {
                0 => return Err(at),
                plus_one => {
                    let raw = plus_one - 1;
                    let (content, _) = self.slot(raw).expect("a table entry before its value");
                    if content == id {
                        return Ok(Idx::from_raw(raw));
                    }
                }
            }
            at = (at + 1) & mask;
        }
    }

    /// Moves a full shard's entries into a table twice its size and makes it the one in use.
    /// Called with the shard locked.
    fn grow(&self, shard: &Shard, level: usize) -> usize {
        assert!(level + 1 < TABLES, "a shard of more than 2^30 values");
        let old = Self::table(shard, level);
        let new = Self::table(shard, level + 1);
        let mask = new.len() - 1;
        for entry in old {
            let plus_one = entry.load(Ordering::Relaxed);
            if plus_one == 0 {
                continue;
            }
            let (content, _) = self.slot(plus_one - 1).expect("a table entry before its value");
            let mut at = Self::home(content) & mask;
            while new[at].load(Ordering::Relaxed) != 0 {
                at = (at + 1) & mask;
            }
            new[at].store(plus_one, Ordering::Relaxed);
        }
        shard.level.store(level + 1, Ordering::Release);
        level + 1
    }
}

impl<T: Send + Sync> Default for Sharded<T> {
    fn default() -> Sharded<T> {
        Sharded::new()
    }
}

impl<T: Send + Sync> Interner<T> for Sharded<T> {
    fn intern(&self, id: ContentId, value: impl FnOnce() -> T) -> Idx<T> {
        if let Some(found) = self.find(&id) {
            return found;
        }
        let value = value();
        let shard = self.shard(&id);
        let mut claimed = shard.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut level = shard.level.load(Ordering::Acquire);
        if let Ok(found) = self.probe(Self::table(shard, level), &id) {
            return found;
        }
        let len = shard.len.load(Ordering::Relaxed);
        if (len as usize + 1) * 2 > TABLE << level {
            level = self.grow(shard, level);
        }
        if claimed.next == claimed.end {
            let start = self.next.fetch_add(BLOCK as u64, Ordering::AcqRel);
            assert!(start + BLOCK as u64 <= u32::MAX as u64, "an interner of 2^32 values");
            *claimed = Claimed { next: start as u32, end: start as u32 + BLOCK };
        }
        let raw = claimed.next;
        claimed.next += 1;
        let (chunk, at) = chunk_of(raw);
        let slots = self.slots[chunk]
            .get_or_init(|| (0..FIRST << chunk).map(|_| OnceLock::new()).collect());
        if slots[at].set((id, value)).is_err() {
            unreachable!("index {raw} handed out twice");
        }
        let table = Self::table(shard, level);
        let Err(empty) = self.probe(table, &id) else { unreachable!("interned under the lock") };
        table[empty].store(raw + 1, Ordering::Release);
        shard.len.store(len + 1, Ordering::Relaxed);
        Idx::from_raw(raw)
    }

    fn get(&self, idx: Idx<T>) -> &T {
        &self.slot(idx.raw()).expect("an index this interner did not hand out").1
    }

    fn content(&self, idx: Idx<T>) -> ContentId {
        self.slot(idx.raw()).expect("an index this interner did not hand out").0
    }

    fn find(&self, id: &ContentId) -> Option<Idx<T>> {
        let shard = self.shard(id);
        let level = shard.level.load(Ordering::Acquire);
        self.probe(Self::table(shard, level), id).ok()
    }

    fn len(&self) -> usize {
        self.shards.iter().map(|shard| shard.len.load(Ordering::Relaxed) as usize).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_part_equal_bytes() {
        assert_ne!(ContentId::of("blaze v2 a 1", b"x"), ContentId::of("blaze v2 b 1", b"x"));
        assert_eq!(ContentId::of("blaze v2 a 1", b"x"), ContentId::of("blaze v2 a 1", b"x"));
    }

    const DOMAINS: [&str; 5] =
        ["blaze v2 a 1", "blaze v2 b 1", "", "blaze v2 key 1", "blaze v2 a domain of some length 7"];

    #[test]
    fn a_hasher_derives_its_domain_key() {
        for domain in DOMAINS {
            for bytes in [&b""[..], b"x", &[7; 1025]] {
                let mut expected = blake3::Hasher::new_derive_key(domain);
                expected.update(bytes);
                let expected = ContentId(*expected.finalize().as_bytes());
                assert_eq!(ContentId::of(domain, bytes), expected, "{domain:?}");
                assert_eq!(ContentId::of(domain, bytes), expected, "{domain:?}, again");
                let named = Domain::new(domain);
                assert_eq!(named.name(), domain);
                assert_eq!(ContentId::of_domain(&named, bytes), expected, "{domain:?}");
            }
        }
    }

    #[test]
    fn of_many_is_of_for_each() {
        let lengths = [0, 1, 63, 64, 65, 1024, 1025, 3000, 2, 64];
        let inputs: Vec<Vec<u8>> =
            lengths.iter().enumerate().map(|(i, &len)| (0..len).map(|b| (b * 7 + i) as u8).collect()).collect();
        let inputs: Vec<&[u8]> = inputs.iter().map(Vec::as_slice).collect();
        for domain in DOMAINS {
            let expected: Vec<ContentId> = inputs.iter().map(|it| ContentId::of(domain, it)).collect();
            assert_eq!(ContentId::of_many(domain, &inputs), expected, "{domain:?}");
        }
        assert_eq!(ContentId::of_many("blaze v2 a 1", &[]), Vec::new());
    }

    #[test]
    fn many_domains_keep_their_own_keys() {
        let names: Vec<&'static str> =
            (0..600).map(|n| &*Box::leak(format!("blaze v2 test domain {n}").into_boxed_str())).collect();
        let first: Vec<ContentId> = names.iter().map(|name| ContentId::of(name, b"x")).collect();
        let threads: Vec<Vec<ContentId>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| names.iter().map(|name| ContentId::of(name, b"x")).collect()))
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        for (name, id) in names.iter().zip(&first) {
            let mut expected = blake3::Hasher::new_derive_key(name);
            expected.update(b"x");
            assert_eq!(*id, ContentId(*expected.finalize().as_bytes()), "{name}");
        }
        for ids in threads {
            assert_eq!(ids, first);
        }
        let copy: &'static str = Box::leak(names[3].to_owned().into_boxed_str());
        assert_eq!(ContentId::of(copy, b"x"), first[3], "the same name at another address");
    }

    #[test]
    fn shards_are_a_power_of_two_of_at_least_four_per_core() {
        let cores = std::thread::available_parallelism().map_or(1, |it| it.get());
        let count = shard_count();
        assert!(count.is_power_of_two() && count >= 4 && count >= 4 * cores, "{count}");
        assert_eq!(Sharded::<u8>::new().shards.len(), count);
    }

    #[test]
    fn a_value_may_intern_into_its_own_interner() {
        let interner = Sharded::<u64>::new();
        let outer = interner.intern(id(1), || *interner.get(interner.intern(id(2), || 2)) + 1);
        assert_eq!(*interner.get(outer), 3);
        assert_eq!(interner.find(&id(2)).map(|it| *interner.get(it)), Some(2));
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn indices_are_dense_but_for_each_shards_last_block() {
        let interner = Sharded::<u64>::new();
        for n in 0..30_000 {
            interner.intern(id(n), || n);
        }
        let end = interner.end();
        let shards = interner.shards.len();
        assert!(end >= interner.len() && end - interner.len() <= shards * BLOCK as usize, "{end}");
        for n in 0..30_000 {
            assert!((interner.find(&id(n)).unwrap().raw() as usize) < end);
        }
    }

    #[test]
    fn parts_are_framed() {
        let ab_c = Hasher::new("blaze v2 test 1").part(b"ab").part(b"c").finish();
        let a_bc = Hasher::new("blaze v2 test 1").part(b"a").part(b"bc").finish();
        assert_ne!(ab_c, a_bc);
    }

    fn id(n: u64) -> ContentId {
        ContentId::of("blaze v2 test 1", &n.to_le_bytes())
    }

    #[test]
    fn chunks_cover_every_index() {
        let mut expected = (0, 0);
        for raw in 0..100_000u32 {
            assert_eq!(chunk_of(raw), expected, "{raw}");
            expected.1 += 1;
            if expected.1 == FIRST << expected.0 {
                expected = (expected.0 + 1, 0);
            }
        }
        assert!(chunk_of(u32::MAX).0 < CHUNKS);
    }

    #[test]
    fn an_interner_maps_both_ways() {
        let interner = Sharded::<u64>::new();
        let indices: Vec<_> = (0..20_000).map(|n| interner.intern(id(n), || n)).collect();
        assert_eq!(interner.len(), 20_000);
        for (n, &idx) in indices.iter().enumerate() {
            let n = n as u64;
            assert_eq!(*interner.get(idx), n);
            assert_eq!(interner.content(idx), id(n));
            assert_eq!(interner.find(&id(n)), Some(idx));
            assert_eq!(interner.intern(id(n), || unreachable!("interned twice")), idx);
        }
        assert_eq!(interner.find(&id(1 << 40)), None);
        assert_eq!(interner.len(), 20_000);
    }

    #[test]
    fn values_never_move() {
        let interner = Sharded::<String>::new();
        let first = interner.intern(id(0), || "zero".to_owned());
        let at = interner.get(first) as *const String;
        for n in 1..50_000 {
            interner.intern(id(n), || n.to_string());
        }
        assert!(std::ptr::eq(at, interner.get(first)));
    }

    #[test]
    fn threads_intern_one_index_per_value() {
        let interner = Sharded::<u64>::new();
        let seen: Vec<Vec<Idx<u64>>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8u64)
                .map(|worker| {
                    let interner = &interner;
                    scope.spawn(move || {
                        (0..10_000u64)
                            .map(|n| (n + worker * 997) % 10_000)
                            .map(|n| interner.intern(id(n), || n))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        assert_eq!(interner.len(), 10_000);
        let mut raws: Vec<u32> = (0..10_000).map(|n| interner.find(&id(n)).unwrap().raw()).collect();
        raws.sort_unstable();
        raws.dedup();
        assert_eq!(raws.len(), 10_000);
        for (worker, indices) in seen.iter().enumerate() {
            for (i, &idx) in indices.iter().enumerate() {
                let n = (i as u64 + worker as u64 * 997) % 10_000;
                assert_eq!(*interner.get(idx), n);
                assert_eq!(interner.find(&id(n)), Some(idx));
            }
        }
    }
}
