//! Dense node numbers for the keys query edges name: rustc's `DepNodeIndex`.
//!
//! A [`DatabaseKeyIndex`] is 12 bytes; an edge names one in 4 by its node, a number given the first
//! time a finished query's edges name the key and kept for the life of the database, so an edge
//! never comes to name another key. Two columns map a node back to its key, in pages that double
//! and never move: 12 bytes a key.
//!
//! The map from a key to its node is a SwissTable probed with `blaze_mem::swiss`
//! (docs/v2/plan/design/hardware.md, "interner and memo lookups"), its slots holding only the
//! node: groups of seven 4-byte slots and a control word of tag bytes in 40 bytes, found with no
//! lock and no write; a tag match compares the key through the node's key columns. A miss takes
//! its shard's lock to look again and insert. A table fills to three quarters, then grows by
//! copying into one twice its size, published once full; the tables it outgrew stay readable, as
//! a reader may be probing one, until [`Nodes::release_outgrown`] frees them with `&mut`. So a
//! key costs its 12 bytes of key columns and 7 to 15 bytes of table. Shards are four times the
//! cores (docs/v2/plan/design/concurrency.md, "Writers shard by hash"), their locks padded apart
//! from what readers load.
//!
//! Every write is an atomic store, so this module has no `unsafe`: a node's key is stored before
//! the node is put in a slot, and a slot before its tag is published with `Release`; a reader that
//! loads the control word with `Acquire` and sees the tag sees both. So whoever learns a node,
//! through the table or an edge published after it, sees its key.

use std::cell::{Cell, OnceCell};

use blaze_mem::swiss::{self, Ctrl};

use crate::Id;
use crate::key::DatabaseKeyIndex;
use crate::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use crate::sync::{Mutex, OnceLock, max_parallelism};
use crate::zalsa::IngredientIndex;

/// Nodes are below this: an edge word keeps its top bit for the edge's kind.
pub(crate) const NODE_LIMIT: u32 = 1 << 31;

/// Slots in a group: tag bytes 0 to 6 of the control word; byte 7 stays 0.
const GROUP: usize = 7;

/// Table `i` of a shard has `1 << i` groups, so 32 levels hold far more than [`NODE_LIMIT`].
const LEVELS: usize = 32;

/// Never fewer shards than this, so a small machine still spreads its writers.
const LEAST_SHARD_BITS: u32 = 4;

/// Key page `p` holds `1 << (FIRST_PAGE_BITS + p)` nodes; 22 pages hold [`NODE_LIMIT`].
const FIRST_PAGE_BITS: u32 = 10;
const PAGES: usize = 22;

/// Entries of a task's cache of key to node.
const CACHE: usize = 1024;

/// Seven slots, each a node written once by the holder of its shard's lock, and their tags.
///
/// `ctrl` byte `i` (from the least significant) is slot `i`'s tag: 0 when empty, `0x80 |` the
/// hash's top seven bits when full. 40 bytes, so a probe touches one line or two adjacent ones.
#[repr(C)]
struct Group {
    ctrl: AtomicU64,
    nodes: [AtomicU32; GROUP],
}

#[cfg(not(feature = "shuttle"))]
const _: () = assert!(size_of::<Group>() == 40);

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
    ids: super::Words<AtomicU64>,
    ingredients: super::Words<AtomicU32>,
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

/// The page of `node` and its offset there.
#[inline]
fn locate(node: u32) -> (usize, usize) {
    let x = node as usize + (1 << FIRST_PAGE_BITS);
    let bits = usize::BITS - 1 - x.leading_zeros();
    ((bits - FIRST_PAGE_BITS) as usize, x - (1 << bits))
}

impl Table {
    fn new(groups: usize) -> Table {
        debug_assert!(groups.is_power_of_two());
        Table {
            groups: (0..groups)
                .map(|_| Group {
                    ctrl: AtomicU64::new(0),
                    nodes: std::array::from_fn(|_| AtomicU32::new(0)),
                })
                // alloc: growth: one table per doubling of the node count, the old one freed at the next revision
                .collect(),
        }
    }

    fn capacity(&self) -> usize {
        self.groups.len() * GROUP
    }

    /// The node of the key, probing groups from the hash's home until one with an empty slot.
    #[inline]
    fn find(&self, keys: &Nodes, hash: u64, id: u64, ingredient: u32) -> Option<u32> {
        let ctrl = |at: usize| Ctrl::<GROUP>(self.groups[at].ctrl.load(Ordering::Acquire));
        swiss::find(hash, swiss::tag(hash), self.groups.len(), ctrl, |at| {
            // Relaxed: the tag's `Acquire` above ordered the slot's and its key's stores
            // before us; only tags published in the loaded word are matched.
            let node = self.groups[at.group].nodes[at.slot].load(Ordering::Relaxed);
            (keys.parts(node) == (id, ingredient)).then_some(node)
        })
    }

    /// Fills the first empty slot from the hash's home group. Called by the holder of the
    /// shard's lock, the only writer, on a table that stays at most three quarters full.
    fn put(&self, hash: u64, node: u32, publish: Ordering) {
        let at = swiss::vacant(hash, self.groups.len(), |at: usize| {
            Ctrl::<GROUP>(self.groups[at].ctrl.load(Ordering::Relaxed))
        });
        let group = &self.groups[at.group];
        group.nodes[at.slot].store(node, Ordering::Relaxed);
        group.ctrl.store(at.tagged(swiss::tag(hash)).0, publish);
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
            // alloc: setup, once per database
            heads: (0..shards).map(|_| Head::new()).collect(),
            // alloc: setup, once per database
            writers: (0..shards).map(|_| Writer(Mutex::new(0))).collect(),
            shard_bits,
            // alloc: setup, once per database
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
            .find(self, hash, id, ingredient)
    }

    /// The node of `key`, given one if it has none.
    #[inline]
    pub(crate) fn node(&self, key: DatabaseKeyIndex) -> u32 {
        let (id, ingredient) = parts(key);
        let hash = hash_parts(id, ingredient);
        let shard = self.shard(hash);
        if let Some(node) = self.heads[shard]
            .current(Ordering::Acquire)
            .and_then(|table| table.find(self, hash, id, ingredient))
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
                // alloc: setup, once per task: its 1024-entry node cache
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
            .and_then(|table| table.find(self, hash, id, ingredient))
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
            .and_then(|table| table.find(self, hash, id, ingredient))
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
                ids: super::zeroed_u64s(len),
                ingredients: super::zeroed_u32s(len),
            }
        });
        page.ids[offset].store(id, Ordering::Relaxed);
        page.ingredients[offset].store(ingredient, Ordering::Relaxed);
        self.table_for_one_more(shard, *len)
            .put(hash, node, Ordering::Release);
        *len += 1;
        node
    }

    /// The table to insert one more key into, at most three quarters full after it. Called with
    /// the shard's lock held.
    fn table_for_one_more(&self, shard: usize, len: usize) -> &Table {
        let head = &self.heads[shard];
        // Relaxed: only holders of the lock we hold store `level`.
        let level = head.level.load(Ordering::Relaxed);
        let current = head.current(Ordering::Relaxed);
        if let Some(table) = current
            && (len + 1) * 4 <= table.capacity() * 3
        {
            return table;
        }
        let grown = Table::new(1 << level);
        if let Some(old) = current {
            // Relaxed: every write to the table was made by a holder of the lock we hold.
            for group in &old.groups {
                for slot in Ctrl::<GROUP>(group.ctrl.load(Ordering::Relaxed)).full() {
                    let node = group.nodes[slot].load(Ordering::Relaxed);
                    let (id, ingredient) = self.parts(node);
                    grown.put(hash_parts(id, ingredient), node, Ordering::Relaxed);
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
        let (id, ingredient) = self.parts(node);
        DatabaseKeyIndex::new(IngredientIndex::new(ingredient), Id::from_bits(id))
    }

    /// The key columns of `node`: its id's bits and its ingredient.
    #[inline]
    fn parts(&self, node: u32) -> (u64, u32) {
        let (page, offset) = locate(node);
        let page = self.pages[page]
            .get()
            .expect("salsa: an edge names a node whose key was never stored");
        (
            page.ids[offset].load(Ordering::Relaxed),
            page.ingredients[offset].load(Ordering::Relaxed),
        )
    }

    /// Frees the tables each shard outgrew. With `&mut`, no reader is probing one.
    pub(crate) fn release_outgrown(&mut self) {
        for head in &mut self.heads {
            let level = head.level.load(Ordering::Relaxed);
            for table in &mut head.tables[..level.saturating_sub(1)] {
                *table = OnceLock::new();
            }
        }
    }

    /// How many keys have a node.
    #[cfg(test)]
    pub(crate) fn len(&self) -> u32 {
        self.next.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
#[cfg(not(feature = "shuttle"))]
mod tests {
    use std::collections::HashMap;

    use blaze_ids::rng::SplitMix64;

    use super::{FIRST_PAGE_BITS, NodeCache, Nodes, locate};
    use crate::Id;
    use crate::key::DatabaseKeyIndex;
    use crate::zalsa::IngredientIndex;

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
        let mut rng = SplitMix64::new(7);
        for step in 0..60_000u32 {
            let k = key(
                rng.below(40) as u32,
                rng.below(3000) as u32,
                rng.below(3) as u32,
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
    fn freeing_outgrown_tables_keeps_every_node() {
        let mut nodes = Nodes::new();
        let keys: Vec<DatabaseKeyIndex> = (0..50_000).map(|i| key(i % 5, i, 0)).collect();
        let first: Vec<u32> = keys[..25_000].iter().map(|&k| nodes.node(k)).collect();
        nodes.release_outgrown();
        let all: Vec<u32> = keys.iter().map(|&k| nodes.node(k)).collect();
        nodes.release_outgrown();
        assert_eq!(&all[..25_000], &first[..]);
        assert_eq!(all, (0..50_000).collect::<Vec<u32>>());
        for (&k, &node) in keys.iter().zip(&all) {
            assert_eq!(nodes.find(k), Some(node));
            assert_eq!(nodes.key(node), k);
        }
        for head in &nodes.heads {
            let level = head.level.load(std::sync::atomic::Ordering::Relaxed);
            assert!(head.tables[..level.saturating_sub(1)].iter().all(|t| t.get().is_none()));
        }
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
                        let mut order: Vec<usize> = (0..keys.len()).collect();
                        SplitMix64::new(t).shuffle(&mut order);
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
