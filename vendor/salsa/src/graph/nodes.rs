//! Dense node numbers for the keys query edges name: rustc's `DepNodeIndex`.
//!
//! A [`DatabaseKeyIndex`] is 12 bytes; an edge names one in 4 by its node, a number given the first
//! time a finished query's edges name the key and kept for the life of the database, so an edge
//! never comes to name another key. Two columns map a node back to its key, in pages that double
//! and never move: 12 bytes a key.
//!
//! The map from a key to its node is the SwissTable of `crates/intern/src/arena.rs`
//! (docs/v2/plan/design/hardware.md, "interner and memo lookups"), with the key and node held in
//! the slot: groups of seven 16-byte slots and a control word of tag bytes on one 128-byte pair of
//! lines, found with no lock and no write. A miss takes its shard's lock to look again and insert.
//! A table grows by copying into one twice its size, published once full; the tables it outgrew
//! stay, as a reader may be probing one, until the database is dropped. Shards are four times the
//! cores (docs/v2/plan/design/concurrency.md, "Writers shard by hash"), their locks padded apart
//! from what readers load.
//!
//! Every write is an atomic store, so this module has no `unsafe`: a slot's key and node are
//! stored before its tag is published with `Release`, and a reader that loads the control word
//! with `Acquire` and sees the tag sees them. A node's key is stored before its slot is published,
//! so whoever learns a node, through the table or an edge published after it, sees its key.

use std::cell::{Cell, OnceCell};

use crate::Id;
use crate::key::DatabaseKeyIndex;
use crate::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use crate::sync::{Mutex, OnceLock, max_parallelism};
use crate::zalsa::IngredientIndex;

/// Nodes are below this: an edge word keeps its top bit for the edge's kind.
pub(crate) const NODE_LIMIT: u32 = 1 << 31;

/// Slots in a group: tag bytes 0 to 6 of the control word; byte 7 stays 0 and is masked out of
/// every match.
const GROUP: usize = 7;
const LOW_BITS: u64 = 0x0001_0101_0101_0101;
const HIGH_BITS: u64 = 0x0080_8080_8080_8080;

/// Table `i` of a shard has `1 << i` groups, so 32 levels hold far more than [`NODE_LIMIT`].
const LEVELS: usize = 32;

/// Never fewer shards than this, so a small machine still spreads its writers.
const LEAST_SHARD_BITS: u32 = 4;

/// Key page `p` holds `1 << (FIRST_PAGE_BITS + p)` nodes; 22 pages hold [`NODE_LIMIT`].
const FIRST_PAGE_BITS: u32 = 10;
const PAGES: usize = 22;

/// Entries of a task's cache of key to node.
const CACHE: usize = 1024;

/// A key and its node. Written once, by the holder of its shard's lock, before its tag.
struct Slot {
    id: AtomicU64,
    ingredient: AtomicU32,
    node: AtomicU32,
}

/// Seven slots and their tags on one pair of cache lines.
///
/// `ctrl` byte `i` (from the least significant) is slot `i`'s tag: 0 when empty, `0x80 |` the
/// hash's top seven bits when full.
#[repr(C, align(128))]
struct Group {
    ctrl: AtomicU64,
    slots: [Slot; GROUP],
}

#[cfg(not(feature = "shuttle"))]
const _: () = assert!(size_of::<Slot>() == 16);
#[cfg(not(feature = "shuttle"))]
const _: () = assert!(size_of::<Group>() == 128);

struct Table {
    groups: Box<[Group]>,
}

/// A shard's tables: `level` of them are published, the last in use.
struct Head {
    level: AtomicUsize,
    tables: [OnceLock<Table>; LEVELS],
}

/// A shard's insert lock and its count of keys, padded so writers of two shards never share a
/// line, nor a writer a reader's.
#[repr(align(128))]
struct Writer(Mutex<usize>);

/// The node to key columns of `1 << (FIRST_PAGE_BITS + p)` nodes.
struct KeyPage {
    ids: Box<[AtomicU64]>,
    ingredients: Box<[AtomicU32]>,
}

pub(crate) struct Nodes {
    heads: Box<[Head]>,
    writers: Box<[Writer]>,
    shard_bits: u32,
    pages: Box<[OnceLock<KeyPage>]>,
    next: AtomicU32,
}

/// A task's direct-mapped cache of key to node, so the keys a task reads again and again cost
/// one line it alone writes.
#[derive(Default)]
pub(crate) struct NodeCache {
    entries: OnceCell<Box<[Cell<CacheEntry>]>>,
}

#[derive(Copy, Clone)]
struct CacheEntry {
    id: u64,
    ingredient: u32,
    node: u32,
}

impl CacheEntry {
    /// No node is `u32::MAX` (they are below [`NODE_LIMIT`]).
    const EMPTY: CacheEntry = CacheEntry {
        id: 0,
        ingredient: 0,
        node: u32::MAX,
    };
}

#[inline]
fn hash_parts(id: u64, ingredient: u32) -> u64 {
    let x = id ^ u64::from(ingredient).rotate_right(20);
    // splitmix64's finalizer, so every bit of the key reaches the probe, the tag and the shard.
    let x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[inline]
fn parts(key: DatabaseKeyIndex) -> (u64, u32) {
    (key.key_index().as_bits(), key.ingredient_index().as_u32())
}

fn tag(hash: u64) -> u64 {
    0x80 | (hash >> 57)
}

/// A byte's high bit set for each slot byte of `word` that may equal `tag`: every equal byte,
/// and rarely the byte above one (the borrow of the subtraction), which the caller checks.
fn matching(word: u64, tag: u64) -> u64 {
    let x = word ^ (LOW_BITS * tag);
    x.wrapping_sub(LOW_BITS) & !x & HIGH_BITS
}

/// A byte's high bit set for each empty slot byte of `word`.
fn empty(word: u64) -> u64 {
    !word & HIGH_BITS
}

/// The page of `node` and its offset there.
#[inline]
fn locate(node: u32) -> (usize, usize) {
    let x = node as usize + (1 << FIRST_PAGE_BITS);
    let bits = usize::BITS - 1 - x.leading_zeros();
    ((bits - FIRST_PAGE_BITS) as usize, x - (1 << bits))
}

impl Slot {
    fn new() -> Slot {
        Slot {
            id: AtomicU64::new(0),
            ingredient: AtomicU32::new(0),
            node: AtomicU32::new(0),
        }
    }
}

impl Table {
    fn new(groups: usize) -> Table {
        debug_assert!(groups.is_power_of_two());
        Table {
            groups: (0..groups)
                .map(|_| Group {
                    ctrl: AtomicU64::new(0),
                    slots: std::array::from_fn(|_| Slot::new()),
                })
                .collect(),
        }
    }

    fn capacity(&self) -> usize {
        self.groups.len() * GROUP
    }

    /// The node of the key, probing groups from the hash's home until one with an empty slot.
    #[inline]
    fn find(&self, hash: u64, id: u64, ingredient: u32) -> Option<u32> {
        let mask = self.groups.len() - 1;
        let tag = tag(hash);
        let mut at = hash as usize & mask;
        loop {
            let group = &self.groups[at];
            let word = group.ctrl.load(Ordering::Acquire);
            let mut hits = matching(word, tag);
            while hits != 0 {
                let byte = (hits.trailing_zeros() / 8) as usize;
                hits &= hits - 1;
                // A byte matched only by the borrow may be a slot not published in `word`.
                if (word >> (byte * 8)) & 0xff != tag {
                    continue;
                }
                // Relaxed: the tag's `Acquire` above ordered the slot's stores before us.
                let slot = &group.slots[byte];
                if slot.id.load(Ordering::Relaxed) == id
                    && slot.ingredient.load(Ordering::Relaxed) == ingredient
                {
                    return Some(slot.node.load(Ordering::Relaxed));
                }
            }
            if empty(word) != 0 {
                return None;
            }
            at = (at + 1) & mask;
        }
    }

    /// Fills the first empty slot from the hash's home group. Called by the holder of the
    /// shard's lock, the only writer, on a table that stays at most half full.
    fn put(&self, hash: u64, id: u64, ingredient: u32, node: u32, publish: Ordering) {
        let mask = self.groups.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let group = &self.groups[at];
            let word = group.ctrl.load(Ordering::Relaxed);
            let free = empty(word);
            if free != 0 {
                let byte = (free.trailing_zeros() / 8) as usize;
                let slot = &group.slots[byte];
                slot.id.store(id, Ordering::Relaxed);
                slot.ingredient.store(ingredient, Ordering::Relaxed);
                slot.node.store(node, Ordering::Relaxed);
                group.ctrl.store(word | (tag(hash) << (byte * 8)), publish);
                return;
            }
            at = (at + 1) & mask;
        }
    }
}

impl Head {
    fn new() -> Head {
        Head {
            level: AtomicUsize::new(0),
            tables: std::array::from_fn(|_| OnceLock::new()),
        }
    }

    /// The table in use, if the shard has one.
    #[inline]
    fn current(&self, order: Ordering) -> Option<&Table> {
        let level = self.level.load(order).checked_sub(1)?;
        Some(
            self.tables[level]
                .get()
                .expect("salsa: a published node table level holds its table"),
        )
    }
}

impl Default for Nodes {
    fn default() -> Self {
        Self::new()
    }
}

impl Nodes {
    pub(crate) fn new() -> Nodes {
        let shard_bits = (max_parallelism() * 4)
            .next_power_of_two()
            .trailing_zeros()
            .max(LEAST_SHARD_BITS);
        let shards = 1usize << shard_bits;
        Nodes {
            heads: (0..shards).map(|_| Head::new()).collect(),
            writers: (0..shards).map(|_| Writer(Mutex::new(0))).collect(),
            shard_bits,
            pages: (0..PAGES).map(|_| OnceLock::new()).collect(),
            next: AtomicU32::new(0),
        }
    }

    #[inline]
    fn shard(&self, hash: u64) -> usize {
        // The probe starts from the hash's low bits and the tag is its top ones; the shard is
        // picked by all of them, mixed.
        (hash.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (u64::BITS - self.shard_bits)) as usize
    }

    /// The node of `key`, if it has one, without a lock or a write.
    #[cfg(test)]
    pub(crate) fn find(&self, key: DatabaseKeyIndex) -> Option<u32> {
        let (id, ingredient) = parts(key);
        let hash = hash_parts(id, ingredient);
        self.heads[self.shard(hash)]
            .current(Ordering::Acquire)?
            .find(hash, id, ingredient)
    }

    /// The node of `key`, given one if it has none.
    #[inline]
    pub(crate) fn node(&self, key: DatabaseKeyIndex) -> u32 {
        let (id, ingredient) = parts(key);
        let hash = hash_parts(id, ingredient);
        let shard = self.shard(hash);
        if let Some(node) = self.heads[shard]
            .current(Ordering::Acquire)
            .and_then(|table| table.find(hash, id, ingredient))
        {
            return node;
        }
        self.insert(shard, hash, id, ingredient)
    }

    /// The node of `key`, through the task's cache.
    #[inline]
    pub(crate) fn node_cached(&self, cache: &NodeCache, key: DatabaseKeyIndex) -> u32 {
        let (id, ingredient) = parts(key);
        let entries = cache.entries.get_or_init(|| {
            (0..CACHE)
                .map(|_| Cell::new(CacheEntry::EMPTY))
                .collect()
        });
        let hash = hash_parts(id, ingredient);
        let entry = &entries[(hash >> 32) as usize & (CACHE - 1)];
        let cached = entry.get();
        if cached.node != CacheEntry::EMPTY.node && cached.id == id && cached.ingredient == ingredient
        {
            return cached.node;
        }
        let shard = self.shard(hash);
        let node = match self.heads[shard]
            .current(Ordering::Acquire)
            .and_then(|table| table.find(hash, id, ingredient))
        {
            Some(node) => node,
            None => self.insert(shard, hash, id, ingredient),
        };
        entry.set(CacheEntry {
            id,
            ingredient,
            node,
        });
        node
    }

    #[cold]
    #[inline(never)]
    fn insert(&self, shard: usize, hash: u64, id: u64, ingredient: u32) -> u32 {
        let mut len = self.writers[shard].0.lock();
        // Another thread may have inserted it between the look and the lock.
        if let Some(node) = self.heads[shard]
            .current(Ordering::Acquire)
            .and_then(|table| table.find(hash, id, ingredient))
        {
            return node;
        }
        let node = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(
            node < NODE_LIMIT,
            "salsa: more than 2^31 distinct query keys were read; an edge has 31 bits to name one"
        );
        let (page, offset) = locate(node);
        let page = self.pages[page].get_or_init(|| {
            let len = 1usize << (FIRST_PAGE_BITS as usize + page);
            KeyPage {
                ids: (0..len).map(|_| AtomicU64::new(0)).collect(),
                ingredients: (0..len).map(|_| AtomicU32::new(0)).collect(),
            }
        });
        page.ids[offset].store(id, Ordering::Relaxed);
        page.ingredients[offset].store(ingredient, Ordering::Relaxed);
        self.table_for_one_more(shard, *len)
            .put(hash, id, ingredient, node, Ordering::Release);
        *len += 1;
        node
    }

    /// The table to insert one more key into, at most half full after it. Called with the
    /// shard's lock held.
    fn table_for_one_more(&self, shard: usize, len: usize) -> &Table {
        let head = &self.heads[shard];
        // Relaxed: only holders of the lock we hold store `level`.
        let level = head.level.load(Ordering::Relaxed);
        let current = head.current(Ordering::Relaxed);
        if let Some(table) = current
            && (len + 1) * 2 <= table.capacity()
        {
            return table;
        }
        let grown = Table::new(1 << level);
        if let Some(old) = current {
            // Relaxed: every write to the table was made by a holder of the lock we hold.
            for group in &old.groups {
                let mut full = group.ctrl.load(Ordering::Relaxed) & HIGH_BITS;
                while full != 0 {
                    let byte = (full.trailing_zeros() / 8) as usize;
                    full &= full - 1;
                    let slot = &group.slots[byte];
                    let id = slot.id.load(Ordering::Relaxed);
                    let ingredient = slot.ingredient.load(Ordering::Relaxed);
                    let node = slot.node.load(Ordering::Relaxed);
                    grown.put(
                        hash_parts(id, ingredient),
                        id,
                        ingredient,
                        node,
                        Ordering::Relaxed,
                    );
                }
            }
        }
        let table = head
            .tables
            .get(level)
            .expect("salsa: a node shard outgrew its last table level")
            .get_or_init(|| grown);
        // Release: a reader that loads the new level sees the table's slots and, through the
        // lock's order, the keys of their nodes.
        head.level.store(level + 1, Ordering::Release);
        table
    }

    /// The key of `node`, which an edge or [`Nodes::node`] gave.
    #[inline]
    pub(crate) fn key(&self, node: u32) -> DatabaseKeyIndex {
        let (page, offset) = locate(node);
        let page = self.pages[page]
            .get()
            .expect("salsa: an edge names a node whose key was never stored");
        let id = page.ids[offset].load(Ordering::Relaxed);
        let ingredient = page.ingredients[offset].load(Ordering::Relaxed);
        DatabaseKeyIndex::new(IngredientIndex::new(ingredient), Id::from_bits(id))
    }

    /// How many keys have a node.
    #[cfg(test)]
    pub(crate) fn len(&self) -> u32 {
        self.next.load(Ordering::Relaxed)
    }
}

#[cfg(all(test, not(feature = "shuttle")))]
mod tests {
    use std::collections::HashMap;

    use super::{FIRST_PAGE_BITS, NodeCache, Nodes, locate};
    use crate::Id;
    use crate::key::DatabaseKeyIndex;
    use crate::zalsa::IngredientIndex;

    fn stream(mut state: u64) -> impl FnMut() -> u64 {
        move || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
    }

    fn key(ingredient: u32, index: u32, generation: u32) -> DatabaseKeyIndex {
        // SAFETY: Test indices are below `Id::MAX_U32`.
        let id = unsafe { Id::from_index(index) }.with_generation(generation);
        DatabaseKeyIndex::new(IngredientIndex::new(ingredient), id)
    }

    #[test]
    fn pages_tile_the_node_space() {
        assert_eq!(locate(0), (0, 0));
        assert_eq!(locate(1023), (0, 1023));
        assert_eq!(locate(1024), (1, 0));
        assert_eq!(locate(3071), (1, 2047));
        assert_eq!(locate(3072), (2, 0));
        let (page, offset) = locate(super::NODE_LIMIT - 1);
        assert!(page < super::PAGES);
        assert!(offset < 1 << (FIRST_PAGE_BITS as usize + page));
    }

    /// The safe reference is a `HashMap` numbering keys in first-seen order.
    #[test]
    fn nodes_agree_with_a_map() {
        let nodes = Nodes::new();
        let cache = NodeCache::default();
        let mut model: HashMap<DatabaseKeyIndex, u32> = HashMap::new();
        let mut next = stream(7);
        for step in 0..60_000u32 {
            let k = key(
                (next() % 40) as u32,
                (next() % 3000) as u32,
                (next() % 3) as u32,
            );
            let fresh = model.len() as u32;
            let want = *model.entry(k).or_insert(fresh);
            let got = if step % 2 == 0 {
                nodes.node(k)
            } else {
                nodes.node_cached(&cache, k)
            };
            assert_eq!(got, want, "{k:?}");
            assert_eq!(nodes.find(k), Some(want));
            assert_eq!(nodes.key(got), k);
        }
        assert_eq!(nodes.len() as usize, model.len());
        assert_eq!(nodes.find(key(41, 1, 0)), None);
    }

    #[test]
    fn colliding_ingredients_and_generations_stay_apart() {
        let nodes = Nodes::new();
        let a = nodes.node(key(1, 5, 0));
        let b = nodes.node(key(2, 5, 0));
        let c = nodes.node(key(1, 5, 1));
        assert_eq!([a, b, c], [0, 1, 2]);
        assert_eq!(nodes.key(c), key(1, 5, 1));
    }

    #[test]
    fn racing_tasks_agree_on_every_node() {
        let nodes = Nodes::new();
        let keys: Vec<DatabaseKeyIndex> = (0..20_000).map(|i| key(i % 7, i, 0)).collect();
        let seen: Vec<Vec<u32>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8u64)
                .map(|t| {
                    let (nodes, keys) = (&nodes, &keys);
                    scope.spawn(move || {
                        let cache = NodeCache::default();
                        let mut next = stream(t);
                        let mut order: Vec<usize> = (0..keys.len()).collect();
                        for i in (1..order.len()).rev() {
                            order.swap(i, (next() % (i as u64 + 1)) as usize);
                        }
                        let mut got = vec![0; keys.len()];
                        for i in order {
                            got[i] = nodes.node_cached(&cache, keys[i]);
                        }
                        got
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        for got in &seen[1..] {
            assert_eq!(got, &seen[0]);
        }
        let mut numbers = seen[0].clone();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), keys.len());
        for (i, &node) in seen[0].iter().enumerate() {
            assert_eq!(nodes.key(node), keys[i]);
        }
    }
}
