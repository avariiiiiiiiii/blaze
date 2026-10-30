//! The database's tables: sharded maps on [`QueryKey`]s, the memos kept of each key, and
//! where each memo's value is.

use std::{
    hash::BuildHasherDefault,
    sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use blaze_core::Mapping;
use blaze_format::SchemaHash;
use blaze_ids::{ContentId, Fingerprint};
use rustc_hash::FxHasher;

use crate::{Edge, QueryKey, QueryKind, Stored};

pub(crate) type FxMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;
pub(crate) type FxSet<K> = std::collections::HashSet<K, BuildHasherDefault<FxHasher>>;

const SHARDS: usize = 64;

/// A map on query keys split in shards, so workers on different keys rarely meet on a lock.
/// A key's input is a hash already: its first byte picks the shard.
pub(crate) struct Shards<V> {
    shards: Box<[RwLock<FxMap<QueryKey, V>>]>,
}

pub(crate) fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<V> Shards<V> {
    pub(crate) fn new() -> Shards<V> {
        Shards { shards: (0..SHARDS).map(|_| RwLock::new(FxMap::default())).collect() }
    }

    pub(crate) fn shard(&self, key: &QueryKey) -> &RwLock<FxMap<QueryKey, V>> {
        &self.shards[key.input.0[0] as usize % SHARDS]
    }

    pub(crate) fn get(&self, key: &QueryKey) -> Option<V>
    where
        V: Clone,
    {
        read(self.shard(key)).get(key).cloned()
    }

    pub(crate) fn contains(&self, key: &QueryKey) -> bool {
        read(self.shard(key)).contains_key(key)
    }

    pub(crate) fn insert(&self, key: QueryKey, value: V) -> Option<V> {
        write(self.shard(&key)).insert(key, value)
    }

    pub(crate) fn insert_if_absent(&self, key: QueryKey, value: impl FnOnce() -> V) {
        if self.contains(&key) {
            return;
        }
        write(self.shard(&key)).entry(key).or_insert_with(value);
    }

    pub(crate) fn for_each(&self, mut f: impl FnMut(&QueryKey, &V)) {
        for shard in self.shards.iter() {
            for (key, value) in read(shard).iter() {
                f(key, value);
            }
        }
    }

    pub(crate) fn retain(&self, mut f: impl FnMut(&QueryKey, &mut V) -> bool) {
        for shard in self.shards.iter() {
            write(shard).retain(|key, value| f(key, value));
        }
    }
}

impl<V: Clone> Shards<V> {
    pub(crate) fn snapshot(&self) -> Vec<(QueryKey, V)> {
        let mut all = Vec::new();
        self.for_each(|key, value| all.push((*key, value.clone())));
        all
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

/// Where a memo's value is. A buffer it leaves is retired, never freed while the database
/// is shared ([`crate::Database::reclaim`]).
pub(crate) enum Value {
    Memory(Arc<Aligned>),
    /// In a mapped file: a cold pack or the image.
    Mapped { mapping: Mapping, at: usize, len: usize, id: ContentId },
    Evicted,
}

/// One result of one key, with what it read. Immutable but for where its value is.
pub(crate) struct MemoData {
    pub(crate) key: QueryKey,
    pub(crate) fingerprint: Fingerprint,
    pub(crate) schema: SchemaHash,
    pub(crate) producer: ContentId,
    pub(crate) edges: Edges,
    pub(crate) value: Mutex<Value>,
}

/// What a value leaving memory hands to the retired list.
pub(crate) enum Left {
    Nothing,
    Buffer(Arc<Aligned>),
}

impl MemoData {
    /// The value's bytes, while they are in memory or mapped. The pointer stays valid for as
    /// long as the database: a buffer or mapping a memo lets go of is retired, and retired
    /// things are only dropped by `&mut Database`.
    pub(crate) fn bytes(&self) -> Option<(*const u8, usize)> {
        match &*lock(&self.value) {
            Value::Memory(buffer) => Some((buffer.bytes().as_ptr(), buffer.len)),
            Value::Mapped { mapping, at, len, .. } => {
                Some((mapping.bytes()[*at..*at + *len].as_ptr(), *len))
            }
            Value::Evicted => None,
        }
    }

    pub(crate) fn memory(&self) -> Option<Arc<Aligned>> {
        match &*lock(&self.value) {
            Value::Memory(buffer) => Some(buffer.clone()),
            _ => None,
        }
    }

    pub(crate) fn stored(&self) -> Stored {
        match &*lock(&self.value) {
            Value::Memory(_) => Stored::Memory,
            Value::Mapped { id, .. } => Stored::Disk(*id),
            Value::Evicted => Stored::Evicted,
        }
    }

    pub(crate) fn evict(&self) -> Left {
        let mut value = lock(&self.value);
        match std::mem::replace(&mut *value, Value::Evicted) {
            Value::Memory(buffer) => Left::Buffer(buffer),
            other => {
                *value = other;
                Left::Nothing
            }
        }
    }

    /// The value moved to `mapping`, if it is still the buffer `was`.
    pub(crate) fn to_disk(
        &self,
        was: &Arc<Aligned>,
        mapping: Mapping,
        at: usize,
        len: usize,
        id: ContentId,
    ) -> Left {
        let mut value = lock(&self.value);
        match &*value {
            Value::Memory(buffer) if Arc::ptr_eq(buffer, was) => {
                let Value::Memory(buffer) =
                    std::mem::replace(&mut *value, Value::Mapped { mapping, at, len, id })
                else {
                    unreachable!()
                };
                Left::Buffer(buffer)
            }
            _ => Left::Nothing,
        }
    }

    /// An evicted value back in memory.
    pub(crate) fn refill(&self, buffer: Aligned) {
        let mut value = lock(&self.value);
        if matches!(*value, Value::Evicted) {
            *value = Value::Memory(Arc::new(buffer));
        }
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
/// can run it again by its [`QueryKey`] alone.
#[derive(Clone, Default)]
pub(crate) struct Slot {
    pub(crate) memos: Vec<Arc<MemoData>>,
    pub(crate) key: Option<Arc<[u8]>>,
}
