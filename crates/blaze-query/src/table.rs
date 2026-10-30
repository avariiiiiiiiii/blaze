//! The database's tables (docs/v2/plan/design/concurrency.md): maps on [`QueryKey`]s a
//! lookup reads with acquire loads alone, the memos kept of each key, and where each memo's
//! value is.
//!
//! - A [`Map`] is [`blaze_ids::shard_count`] shards, each one open-addressed table of
//!   pointers to immutable entries. A writer publishes an entry with one release store; a
//!   reader loads the table and probes it, writing nothing.
//! - A changed value is a new entry swapped into the slot; a removed one leaves a tombstone. A
//!   table that fills is copied into a new one sized for what is live, published with one
//!   release store and never rehashed in place.
//! - What a writer unlinks (an entry, a table, a memo's value) goes to a
//!   [`blaze_reclaim::Limbo`] and is freed once every task that could have loaded it has
//!   ended. A reader proves it is in a task with a [`Guard`] borrowed for as long as it holds
//!   what it read.

#[allow(clippy::disallowed_types)]
use std::sync::{Mutex, MutexGuard};
use std::{
    hash::BuildHasherDefault,
    marker::PhantomData,
    ptr,
    sync::{
        Arc, OnceLock, PoisonError,
        atomic::{AtomicPtr, AtomicU32, Ordering},
    },
};

use blaze_core::Mapping;
use blaze_format::SchemaHash;
use blaze_ids::{ContentId, Fingerprint};
use blaze_reclaim::{Grace, Guard, Limbo};
use rustc_hash::FxHasher;

use crate::{Edge, QueryKey, QueryKind, Stored};

pub(crate) type FxMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;
pub(crate) type FxSet<K> = std::collections::HashSet<K, BuildHasherDefault<FxHasher>>;

/// A lock held for a few loads and stores: claiming or swapping a slot, copying a table to
/// grow it, taking a tag, holding what was unlinked. Nothing that holds it calls a query,
/// the tree or another lock, so it is never held across a call into the database.
pub(crate) struct ShortLock<T> {
    #[allow(clippy::disallowed_types)]
    inner: Mutex<T>,
}

#[allow(clippy::disallowed_types)]
impl<T> ShortLock<T> {
    pub(crate) fn new(value: T) -> ShortLock<T> {
        ShortLock { inner: Mutex::new(value) }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: Default> Default for ShortLock<T> {
    fn default() -> ShortLock<T> {
        ShortLock::new(T::default())
    }
}

/// How many unlinked things a shard holds before it frees those past their grace.
const COLLECT_AT: usize = 16;
const FIRST_TABLE: usize = 16;

/// One key and its value, never changed once published.
pub(crate) struct Entry<V> {
    pub(crate) key: QueryKey,
    pub(crate) value: V,
}

struct Table<V> {
    mask: usize,
    /// Null is empty, [`tomb`] a removed entry; anything else an entry this table owns.
    places: Box<[AtomicPtr<Entry<V>>]>,
}

/// The pointer a removed entry's place holds: the address of no allocation.
fn tomb<V>() -> *mut Entry<V> {
    ptr::dangling_mut()
}

impl<V> Table<V> {
    fn new(len: usize) -> Box<Table<V>> {
        debug_assert!(len.is_power_of_two());
        let places = (0..len).map(|_| AtomicPtr::new(ptr::null_mut())).collect();
        Box::new(Table { mask: len - 1, places })
    }

    /// Where `key` is, by plain probing.
    fn find(&self, key: &QueryKey) -> Option<(usize, *mut Entry<V>)> {
        let mut at = home(key) & self.mask;
        loop {
            let entry = self.places[at].load(Ordering::Acquire);
            if entry.is_null() {
                return None;
            }
            // SAFETY: a non-null, non-tomb place points at an entry this table owns, freed only
            // after it is unlinked and a grace has passed; the caller is in a task that loaded
            // this table, or holds the shard's lock.
            if entry != tomb() && unsafe { &(*entry).key } == key {
                return Some((at, entry));
            }
            at = (at + 1) & self.mask;
        }
    }

    /// Where `key` is, or the first place it may be put: a tomb or an empty one.
    fn place_for(&self, key: &QueryKey) -> Result<(usize, *mut Entry<V>), usize> {
        let mut at = home(key) & self.mask;
        let mut free = None;
        loop {
            let entry = self.places[at].load(Ordering::Relaxed);
            if entry.is_null() {
                return Err(free.unwrap_or(at));
            }
            if entry == tomb() {
                free.get_or_insert(at);
            // SAFETY: as in `Table::find`; the caller holds the shard's lock.
            } else if unsafe { &(*entry).key } == key {
                return Ok((at, entry));
            }
            at = (at + 1) & self.mask;
        }
    }

    fn live(&self) -> impl Iterator<Item = *mut Entry<V>> + '_ {
        self.places
            .iter()
            .map(|place| place.load(Ordering::Acquire))
            .filter(|entry| !entry.is_null() && *entry != tomb())
    }
}

/// A shard's writer state, under its lock.
#[derive(Default)]
struct Writer {
    live: usize,
    /// Live entries and tombs: what fills the table.
    used: usize,
    limbo: Limbo,
}

impl Writer {
    fn hold<V: Send + 'static>(&mut self, grace: Grace, gone: impl IntoIterator<Item = *mut Entry<V>>) {
        for entry in gone {
            // SAFETY: `entry` was a table's and is unlinked from it under the shard's lock, so
            // this is its one owner; a task that loaded it before ended before `grace` passes.
            self.limbo.hold(grace, unsafe { Box::from_raw(entry) });
        }
        if self.limbo.len() >= COLLECT_AT {
            self.limbo.collect();
        }
    }
}

/// One cache line pair per shard, so writers to neighbouring shards don't share a line.
#[repr(align(128))]
struct Shard<V> {
    writer: ShortLock<Writer>,
    /// Null until the first insert.
    table: AtomicPtr<Table<V>>,
}

/// Bits of the key its shard was not picked by, mixed with its kind: kinds of one input share
/// a shard.
fn home(key: &QueryKey) -> usize {
    let mut word = [0; 8];
    word.copy_from_slice(&key.input.0[8..16]);
    (u64::from_le_bytes(word) ^ u64::from(key.kind.0).wrapping_mul(0x9e37_79b9_7f4a_7c15)) as usize
}

/// What [`Map::edit_all`] does to one entry.
pub(crate) enum Edit<V> {
    Keep,
    Remove,
    Replace(V),
}

/// A map on query keys whose lookups take no lock and write nothing shared.
pub(crate) struct Map<V> {
    shards: Box<[Shard<V>]>,
    mask: usize,
    _owns: PhantomData<Box<Entry<V>>>,
}

impl<V: Send + Sync + 'static> Map<V> {
    pub(crate) fn new() -> Map<V> {
        let count = blaze_ids::shard_count();
        Map {
            shards: (0..count)
                .map(|_| Shard { writer: ShortLock::default(), table: AtomicPtr::new(ptr::null_mut()) })
                .collect(),
            mask: count - 1,
            _owns: PhantomData,
        }
    }

    fn shard(&self, key: &QueryKey) -> &Shard<V> {
        let mut word = [0; 4];
        word.copy_from_slice(&key.input.0[..4]);
        &self.shards[u32::from_le_bytes(word) as usize & self.mask]
    }

    fn table<'g>(shard: &'g Shard<V>, _task: &'g Guard) -> Option<&'g Table<V>> {
        let table = shard.table.load(Ordering::Acquire);
        // SAFETY: a table published in a shard is freed only after a newer one replaced it and
        // a grace passed, or by `&mut`: `_task` keeps this thread in the task that loaded it.
        (!table.is_null()).then(|| unsafe { &*table })
    }

    fn entry<'g>(&'g self, task: &'g Guard, key: &QueryKey) -> Option<&'g Entry<V>> {
        let (_, entry) = Self::table(self.shard(key), task)?.find(key)?;
        // SAFETY: as in `Map::table`: entries are unlinked before a grace and freed after it.
        Some(unsafe { &*entry })
    }

    pub(crate) fn get<'g>(&'g self, task: &'g Guard, key: &QueryKey) -> Option<&'g V> {
        self.entry(task, key).map(|entry| &entry.value)
    }

    /// The shard's table, with a place for one more entry.
    fn room<'s>(shard: &'s Shard<V>, writer: &mut Writer) -> &'s Table<V> {
        let current = shard.table.load(Ordering::Acquire);
        if current.is_null() {
            let table = Box::into_raw(Table::new(FIRST_TABLE));
            shard.table.store(table, Ordering::Release);
            // SAFETY: just published; freed only through the shard's limbo or by `&mut`.
            return unsafe { &*table };
        }
        // SAFETY: the current table, which only this lock's holder replaces.
        let old = unsafe { &*current };
        if (writer.used + 1) * 2 <= old.places.len() {
            return old;
        }
        let new = Table::new(((writer.live + 1) * 4).next_power_of_two().max(FIRST_TABLE));
        for entry in old.live() {
            // SAFETY: a live entry of the current table, under the lock.
            let key = unsafe { &(*entry).key };
            let mut at = home(key) & new.mask;
            while !new.places[at].load(Ordering::Relaxed).is_null() {
                at = (at + 1) & new.mask;
            }
            new.places[at].store(entry, Ordering::Relaxed);
        }
        let new = Box::into_raw(new);
        shard.table.store(new, Ordering::Release);
        writer.used = writer.live;
        // SAFETY: `current` is unlinked; its entries moved to `new`, so the old table's drop
        // frees only its places.
        writer.limbo.hold(Grace::start(), unsafe { Box::from_raw(current) });
        // SAFETY: just published, as above.
        unsafe { &*new }
    }

    /// Publishes `key`'s entry under the shard's lock, unlinking the one it replaces.
    fn publish<'s>(shard: &'s Shard<V>, writer: &mut Writer, key: QueryKey, value: V) -> &'s Entry<V> {
        let table = Self::room(shard, writer);
        let entry = Box::into_raw(Box::new(Entry { key, value }));
        match table.place_for(&key) {
            Ok((at, old)) => {
                table.places[at].store(entry, Ordering::Release);
                writer.hold(Grace::start(), [old]);
            }
            Err(at) => {
                if table.places[at].load(Ordering::Relaxed).is_null() {
                    writer.used += 1;
                }
                writer.live += 1;
                table.places[at].store(entry, Ordering::Release);
            }
        }
        // SAFETY: published in the current table; freed only after it is unlinked and a grace
        // has passed.
        unsafe { &*entry }
    }

    /// Sets `key`'s value; a reader still holding the old one keeps it until its task ends.
    pub(crate) fn insert<'g>(&'g self, _task: &'g Guard, key: QueryKey, value: V) -> &'g V {
        let shard = self.shard(&key);
        let mut writer = shard.writer.lock();
        &Self::publish(shard, &mut writer, key, value).value
    }

    /// `key`'s value, or `make()`'s, made before the shard is locked: a racing writer's wins
    /// and `make()`'s is dropped.
    pub(crate) fn get_or_insert_with<'g>(
        &'g self,
        task: &'g Guard,
        key: QueryKey,
        make: impl FnOnce() -> V,
    ) -> &'g V {
        if let Some(found) = self.get(task, &key) {
            return found;
        }
        let value = make();
        let shard = self.shard(&key);
        let mut writer = shard.writer.lock();
        if let Some(found) = self.get(task, &key) {
            return found;
        }
        &Self::publish(shard, &mut writer, key, value).value
    }

    /// Replaces `key`'s value with what `change` makes of it (`None`: leave it), made without
    /// the lock and made again if another writer changed the entry meanwhile.
    pub(crate) fn update(
        &self,
        task: &Guard,
        key: QueryKey,
        mut change: impl FnMut(Option<&V>) -> Option<V>,
    ) {
        let shard = self.shard(&key);
        loop {
            let seen = self.entry(task, &key);
            let Some(value) = change(seen.map(|entry| &entry.value)) else { return };
            let mut writer = shard.writer.lock();
            let now = self.entry(task, &key).map(ptr::from_ref);
            if now == seen.map(ptr::from_ref) {
                Self::publish(shard, &mut writer, key, value);
                return;
            }
        }
    }

    /// Every entry, as of when each shard's table is loaded; one published meanwhile may be
    /// missed.
    pub(crate) fn for_each<'g>(&'g self, task: &'g Guard, mut f: impl FnMut(&'g QueryKey, &'g V)) {
        for shard in self.shards.iter() {
            let Some(table) = Self::table(shard, task) else { continue };
            for entry in table.live() {
                // SAFETY: as in `Map::entry`.
                let entry = unsafe { &*entry };
                f(&entry.key, &entry.value);
            }
        }
    }

    /// Keeps, removes or replaces each entry as `edit` says, decided without the lock; an entry
    /// another writer changed meanwhile is left as it is.
    pub(crate) fn edit_all(&self, task: &Guard, mut edit: impl FnMut(&QueryKey, &V) -> Edit<V>) {
        for shard in self.shards.iter() {
            let Some(table) = Self::table(shard, task) else { continue };
            let mut edits = Vec::new();
            for entry in table.live() {
                // SAFETY: as in `Map::entry`.
                let seen = unsafe { &*entry };
                match edit(&seen.key, &seen.value) {
                    Edit::Keep => {}
                    other => edits.push((entry, seen.key, other)),
                }
            }
            if edits.is_empty() {
                continue;
            }
            let mut writer = shard.writer.lock();
            let mut gone = Vec::new();
            for (seen, key, edit) in edits {
                let current = shard.table.load(Ordering::Acquire);
                // SAFETY: a table exists: `seen` was in one, and tables are only ever replaced.
                let current = unsafe { &*current };
                let Some((at, now)) = current.find(&key) else { continue };
                if now != seen {
                    continue;
                }
                match edit {
                    Edit::Keep => {}
                    Edit::Remove => {
                        current.places[at].store(tomb(), Ordering::Release);
                        writer.live -= 1;
                        gone.push(seen);
                    }
                    Edit::Replace(value) => {
                        let entry = Box::into_raw(Box::new(Entry { key, value }));
                        current.places[at].store(entry, Ordering::Release);
                        gone.push(seen);
                    }
                }
            }
            if !gone.is_empty() {
                writer.hold(Grace::start(), gone);
            }
        }
    }

    pub(crate) fn retain(&self, task: &Guard, mut keep: impl FnMut(&QueryKey, &V) -> bool) {
        self.edit_all(task, |key, value| if keep(key, value) { Edit::Keep } else { Edit::Remove });
    }

    /// Frees what each shard unlinked whose grace has passed.
    pub(crate) fn collect(&self) {
        for shard in self.shards.iter() {
            let mut writer = shard.writer.lock();
            if !writer.limbo.is_empty() {
                writer.limbo.collect();
            }
        }
    }

    /// How many unlinked things wait for their grace.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.shards.iter().map(|shard| shard.writer.lock().limbo.len()).sum()
    }

    /// Frees everything unlinked: no reader can hold it with the map borrowed `&mut`.
    pub(crate) fn reclaim(&mut self) {
        for shard in self.shards.iter_mut() {
            shard.writer.get_mut().limbo = Limbo::new();
        }
    }
}

impl<V: Send + Sync + Clone + 'static> Map<V> {
    pub(crate) fn snapshot(&self, task: &Guard) -> Vec<(QueryKey, V)> {
        let mut all = Vec::new();
        self.for_each(task, |key, value| all.push((*key, value.clone())));
        all
    }
}

impl<V> Drop for Map<V> {
    fn drop(&mut self) {
        for shard in self.shards.iter_mut() {
            let table = *shard.table.get_mut();
            if table.is_null() {
                continue;
            }
            // SAFETY: the current table, owned by the map; `&mut` excludes every reader.
            let mut table = unsafe { Box::from_raw(table) };
            for place in table.places.iter_mut() {
                let entry = *place.get_mut();
                if !entry.is_null() && entry != tomb() {
                    // SAFETY: a live entry of the current table, which owns it.
                    drop(unsafe { Box::from_raw(entry) });
                }
            }
        }
    }
}

/// Chunk `k` of a [`Column`] holds `COLUMN_FIRST << k` places: 29 hold every `u32`.
const COLUMN_FIRST: usize = 16;
const COLUMN_CHUNKS: usize = 29;

fn column_place(index: u32) -> (usize, usize) {
    let biased = index as usize / COLUMN_FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, index as usize - COLUMN_FIRST * ((1 << chunk) - 1))
}

/// Values by dense index, each published once with a release store and taken out at most
/// once: a lookup is two acquire loads. Chunks never move.
pub(crate) struct Column<T> {
    chunks: [OnceLock<Box<[AtomicPtr<T>]>>; COLUMN_CHUNKS],
    next: AtomicU32,
    _owns: PhantomData<Box<T>>,
}

impl<T: Send + Sync + 'static> Column<T> {
    pub(crate) fn new() -> Column<T> {
        Column { chunks: std::array::from_fn(|_| OnceLock::new()), next: AtomicU32::new(0), _owns: PhantomData }
    }

    fn place(&self, index: u32) -> &AtomicPtr<T> {
        let (chunk, at) = column_place(index);
        &self.chunks[chunk].get_or_init(|| {
            (0..COLUMN_FIRST << chunk).map(|_| AtomicPtr::new(ptr::null_mut())).collect()
        })[at]
    }

    /// Appends `make(index)` at the next index.
    pub(crate) fn push(&self, make: impl FnOnce(u32) -> T) -> u32 {
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(index < u32::MAX, "a column of 2^32 values");
        self.place(index).store(Box::into_raw(Box::new(make(index))), Ordering::Release);
        index
    }

    pub(crate) fn get<'g>(&'g self, _task: &'g Guard, index: u32) -> Option<&'g T> {
        let (chunk, at) = column_place(index);
        let value = self.chunks[chunk].get()?.get(at)?.load(Ordering::Acquire);
        // SAFETY: a published value is freed only after `take` unlinked it and the caller's
        // limbo let a grace pass, or by `&mut`; `_task` keeps this thread in its task.
        (!value.is_null()).then(|| unsafe { &*value })
    }

    /// Unlinks the value at `index`, for the caller to hold in a limbo: a reader may still
    /// have it.
    pub(crate) fn take(&self, index: u32) -> Option<Box<T>> {
        let (chunk, at) = column_place(index);
        let value = self.chunks[chunk].get()?.get(at)?.swap(ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: published by `push` from `Box::into_raw`; the swap made this its one owner.
        (!value.is_null()).then(|| unsafe { Box::from_raw(value) })
    }

    pub(crate) fn for_each<'g>(&'g self, task: &'g Guard, mut f: impl FnMut(&'g T)) {
        for index in 0..self.next.load(Ordering::Acquire) {
            if let Some(value) = self.get(task, index) {
                f(value);
            }
        }
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.chunks.iter_mut().filter_map(OnceLock::get_mut).flat_map(|chunk| chunk.iter_mut()).filter_map(
            |place| {
                let value = *place.get_mut();
                // SAFETY: published from `Box::into_raw` and owned by the column; `&mut`
                // excludes readers.
                (!value.is_null()).then(|| unsafe { &mut *value })
            },
        )
    }

    /// Every value, taken out, and the next index; for [`Column::restore`].
    pub(crate) fn take_all(&mut self) -> (u32, Vec<Box<T>>) {
        let next = *self.next.get_mut();
        (next, (0..next).filter_map(|index| self.take(index)).collect())
    }

    /// `values` at the indices `index_of` gives, the next index pushed `next`.
    pub(crate) fn restore(&mut self, next: u32, values: Vec<Box<T>>, index_of: impl Fn(&T) -> u32) {
        *self.next.get_mut() = next;
        for value in values {
            let index = index_of(&value);
            assert!(index < next, "a value restored past the column's end");
            let old = self.place(index).swap(Box::into_raw(value), Ordering::AcqRel);
            assert!(old.is_null(), "two values restored at index {index}");
        }
    }
}

impl<T> Drop for Column<T> {
    fn drop(&mut self) {
        for chunk in self.chunks.iter_mut().filter_map(OnceLock::get_mut) {
            for place in chunk.iter_mut() {
                let value = *place.get_mut();
                if !value.is_null() {
                    // SAFETY: published from `Box::into_raw` and never taken; `&mut` excludes
                    // readers.
                    drop(unsafe { Box::from_raw(value) });
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Block([u8; 16]);

/// Bytes at a 16-byte-aligned address ([`blaze_format::ALIGN`]), as a value read in place
/// needs.
pub(crate) struct Aligned {
    blocks: Box<[Block]>,
    len: usize,
}

impl Aligned {
    pub(crate) fn copy(bytes: &[u8]) -> Aligned {
        let mut blocks = vec![Block([0; 16]); bytes.len().div_ceil(16)].into_boxed_slice();
        for (block, chunk) in blocks.iter_mut().zip(bytes.chunks(16)) {
            block.0[..chunk.len()].copy_from_slice(chunk);
        }
        Aligned { blocks, len: bytes.len() }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: `blocks` is `len.div_ceil(16)` blocks of 16 initialized bytes with no
        // padding (`Block` is a `[u8; 16]`), so its first `len` bytes are in bounds.
        unsafe { std::slice::from_raw_parts(self.blocks.as_ptr().cast::<u8>(), self.len) }
    }
}

impl AsRef<[u8]> for Aligned {
    fn as_ref(&self) -> &[u8] {
        self.bytes()
    }
}

/// A memo's dependency edges: its own, or read in place from the image they came from.
pub(crate) enum Edges {
    Owned(Box<[Edge]>),
    Mapped { image: Mapping, at: usize, len: usize },
}

pub(crate) const EDGE_RECORD: usize = 56;

pub(crate) fn decode_edge(bytes: &[u8]) -> Edge {
    let kind = u16::from_le_bytes([bytes[0], bytes[1]]);
    let mut input = [0; 32];
    input.copy_from_slice(&bytes[8..40]);
    let mut saw = [0; 16];
    saw.copy_from_slice(&bytes[40..56]);
    Edge {
        read: QueryKey { kind: QueryKind(kind), input: ContentId(input) },
        saw: Fingerprint(u128::from_le_bytes(saw)),
    }
}

impl Edges {
    pub(crate) fn len(&self) -> usize {
        match self {
            Edges::Owned(edges) => edges.len(),
            Edges::Mapped { len, .. } => *len,
        }
    }

    pub(crate) fn get(&self, index: usize) -> Edge {
        match self {
            Edges::Owned(edges) => edges[index],
            Edges::Mapped { image, at, len } => {
                assert!(index < *len);
                let from = at + index * EDGE_RECORD;
                decode_edge(&image.bytes()[from..from + EDGE_RECORD])
            }
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = Edge> + '_ {
        (0..self.len()).map(|index| self.get(index))
    }

    pub(crate) fn to_vec(&self) -> Vec<Edge> {
        self.iter().collect()
    }
}

/// Where a memo's value is, when it has one: never changed once published.
pub(crate) enum Value {
    Memory(Aligned),
    /// In a mapped file: a cold pack or the image.
    Mapped { mapping: Mapping, at: usize, len: usize, id: ContentId },
}

impl Value {
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Value::Memory(buffer) => buffer.bytes(),
            Value::Mapped { mapping, at, len, .. } => &mapping.bytes()[*at..*at + *len],
        }
    }
}

/// A memo's [`Value`], or none for an evicted one: one published pointer, swapped whole.
/// The pointer is an `Arc`'s, so a writer that took a count (a cold write in flight) knows
/// the value it saw is still that one when it swaps; readers take no count.
pub(crate) struct ValueCell {
    current: AtomicPtr<Value>,
}

fn into_raw(value: Option<Arc<Value>>) -> *mut Value {
    value.map_or(ptr::null_mut(), |value| Arc::into_raw(value).cast_mut())
}

impl ValueCell {
    pub(crate) fn new(value: Option<Value>) -> ValueCell {
        ValueCell { current: AtomicPtr::new(into_raw(value.map(Arc::new))) }
    }

    pub(crate) fn load<'g>(&'g self, _task: &'g Guard) -> Option<&'g Value> {
        let value = self.current.load(Ordering::Acquire);
        // SAFETY: a published value is unlinked by a swap, which hands its count to the
        // database's limbo, freed after every task that could have loaded it ended; `_task`
        // keeps this thread in its task. The cell itself lives as long as its memo, which the
        // caller reached through a guarded table.
        (!value.is_null()).then(|| unsafe { &*value })
    }

    /// The value, with a count of its own, while it is in memory.
    pub(crate) fn memory(&self, task: &Guard) -> Option<Arc<Value>> {
        let value = self.load(task)?;
        if !matches!(value, Value::Memory(_)) {
            return None;
        }
        let raw = ptr::from_ref(value);
        // SAFETY: `raw` came from `Arc::into_raw` and is alive while `task` is: one more count.
        unsafe {
            Arc::increment_strong_count(raw);
            Some(Arc::from_raw(raw))
        }
    }

    /// Swaps `was` (null: none) for `now`; the value let go of, for the limbo, or `None` when
    /// the cell no longer holds `was`.
    fn swap(&self, was: *const Value, now: Option<Arc<Value>>) -> Option<Option<Arc<Value>>> {
        let now = into_raw(now);
        match self.current.compare_exchange(was.cast_mut(), now, Ordering::AcqRel, Ordering::Acquire) {
            // SAFETY: the cell's count of `was` is now the caller's.
            Ok(old) => Some((!old.is_null()).then(|| unsafe { Arc::from_raw(old) })),
            Err(_) => {
                if !now.is_null() {
                    // SAFETY: `now` was never published.
                    drop(unsafe { Arc::from_raw(now) });
                }
                None
            }
        }
    }
}

impl Drop for ValueCell {
    fn drop(&mut self) {
        let value = *self.current.get_mut();
        if !value.is_null() {
            // SAFETY: the cell's own count; `&mut` excludes readers.
            drop(unsafe { Arc::from_raw(value) });
        }
    }
}

/// One result of one key, with what it read. Immutable but for its [`ValueCell`].
pub(crate) struct MemoData {
    pub(crate) key: QueryKey,
    pub(crate) fingerprint: Fingerprint,
    pub(crate) schema: SchemaHash,
    pub(crate) producer: ContentId,
    pub(crate) edges: Edges,
    pub(crate) value: ValueCell,
}

impl MemoData {
    /// The value's bytes, while it is in memory or mapped.
    pub(crate) fn bytes<'g>(&'g self, task: &'g Guard) -> Option<&'g [u8]> {
        self.value.load(task).map(Value::bytes)
    }

    pub(crate) fn stored(&self, task: &Guard) -> Stored {
        match self.value.load(task) {
            Some(Value::Memory(_)) => Stored::Memory,
            Some(Value::Mapped { id, .. }) => Stored::Disk(*id),
            None => Stored::Evicted,
        }
    }

    /// Takes the value out of memory; the buffer let go of, for the limbo.
    pub(crate) fn evict(&self, task: &Guard) -> Option<Arc<Value>> {
        loop {
            let seen = self.value.load(task)?;
            if !matches!(seen, Value::Memory(_)) {
                return None;
            }
            if let Some(old) = self.value.swap(ptr::from_ref(seen), None) {
                return old;
            }
        }
    }

    /// The value moved to `mapping`, if it is still the buffer `was`; the buffer let go of.
    pub(crate) fn to_disk(
        &self,
        was: &Arc<Value>,
        mapping: Mapping,
        at: usize,
        len: usize,
        id: ContentId,
    ) -> Option<Arc<Value>> {
        self.value.swap(Arc::as_ptr(was), Some(Arc::new(Value::Mapped { mapping, at, len, id }))).flatten()
    }

    /// An evicted value back in memory; a racing refill's wins.
    pub(crate) fn refill(&self, buffer: Aligned) {
        let refilled = self.value.swap(ptr::null(), Some(Arc::new(Value::Memory(buffer))));
        debug_assert!(refilled.is_none_or(|old| old.is_none()));
    }

    /// Whether `other` is this memo: the same key, value, producer, schema and edges.
    pub(crate) fn same(&self, other: &MemoData) -> bool {
        self.key == other.key
            && self.fingerprint == other.fingerprint
            && self.schema == other.schema
            && self.producer == other.producer
            && self.edges.len() == other.edges.len()
            && self.edges.iter().zip(other.edges.iter()).all(|(a, b)| a == b)
    }
}

/// What the database keeps of one key: its memos (the newest last) and its key's bytes, so it
/// can run it again by its [`QueryKey`] alone. A changed slot is a new one swapped in.
#[derive(Clone, Default)]
pub(crate) struct Slot {
    pub(crate) memos: Vec<Arc<MemoData>>,
    pub(crate) key: Option<Arc<[u8]>>,
}

#[cfg(test)]
mod tests;
