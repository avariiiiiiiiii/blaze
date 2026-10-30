use std::any::TypeId;
use std::borrow::Cow;
use std::cell::UnsafeCell;
use std::fmt;
use std::hash::{BuildHasher, Hash};
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use rustc_hash::FxBuildHasher;
use smallvec::SmallVec;

use crate::durability::{AtomicDurability, Durability};
use crate::function::VerifyResult;
use crate::hash::{FxHashSet, FxIndexSet};
use crate::id::{AsId, FromId};
use crate::ingredient::Ingredient;
use crate::plumbing::{self, Jar, ZalsaLocal};
use crate::revision::AtomicRevision;
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sync::{Arc, Mutex, max_parallelism};
use crate::table::Slot;
use crate::table::memo::{MemoTable, MemoTableTypes, MemoTableWithTypesMut};
use crate::zalsa::{IngredientIndex, JarKind, Zalsa};
use crate::zalsa_local::QueryEdge;
use crate::{DatabaseKeyIndex, Event, EventKind, Id, Revision};

mod table;

use self::table::{Locked, ShardedTable};

#[cfg(not(test))]
const DEFAULT_REVISIONS: usize = 3;

// More aggressive garbage collection by default when testing.
#[cfg(test)]
const DEFAULT_REVISIONS: usize = 1;

/// How many of a shard's values one reuse sweep looks at before allocating instead.
const SWEEP: usize = 8;

/// Trait that defines the key properties of an interned struct.
///
/// Implemented by the `#[salsa::interned]` macro when applied to
/// a struct.
///
/// # Safety
///
/// For every lifetime `'db`, `Fields<'db>` must be safe for Salsa to retain
/// as `Fields<'static>` and later expose with the lifetime of a database
/// borrow. Both types must have identical layouts and validity invariants.
pub unsafe trait Configuration: Sized + 'static {
    const LOCATION: crate::ingredient::Location;
    const DEBUG_NAME: &'static str;

    /// Whether this struct should be persisted with the database.
    const PERSIST: bool;

    // The minimum number of revisions that must pass before a stale value is garbage collected.
    const REVISIONS: NonZeroUsize = NonZeroUsize::new(DEFAULT_REVISIONS).unwrap();

    /// The fields of the struct being interned.
    type Fields<'db>: InternedData;

    /// The end user struct
    type Struct<'db>: Copy + FromId + AsId;

    /// Returns the size of any heap allocations in the output value, in bytes.
    fn heap_size(_value: &Self::Fields<'_>) -> Option<usize> {
        None
    }

    /// Serialize the fields using `serde`.
    ///
    /// Panics if the value is not persistable, i.e. `Configuration::PERSIST` is `false`.
    fn serialize<S>(value: &Self::Fields<'_>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: plumbing::serde::Serializer;

    /// Deserialize the fields using `serde`.
    ///
    /// Panics if the value is not persistable, i.e. `Configuration::PERSIST` is `false`.
    fn deserialize<'de, D>(deserializer: D) -> Result<Self::Fields<'static>, D::Error>
    where
        D: plumbing::serde::Deserializer<'de>;
}

pub trait InternedData: Sized + Eq + Hash + Clone + Sync + Send {}
impl<T: Eq + Hash + Clone + Sync + Send> InternedData for T {}

pub struct JarImpl<C: Configuration> {
    phantom: PhantomData<C>,
}

/// The interned ingredient hashes values of type `C::Fields` to produce an `Id`.
///
/// It used to store interned structs but also to store the ID fields of a tracked struct.
/// Interned values are garbage collected and their memory reused: a CLOCK sweep over each
/// shard's values picks one not interned for the configured number of revisions.
///
/// A hit takes no lock: the key map is read lock-free ([`table`]), and a value's
/// `last_interned_at` word ([`InternedAt`]) carries the protocol that keeps its fields from being
/// rewritten while a reader compares them. Only a miss locks the key's shard.
pub struct IngredientImpl<C: Configuration> {
    /// Index of this ingredient in the database (used to construct database-IDs, etc).
    ingredient_index: IngredientIndex,

    /// A hasher for the sharded ID maps.
    hasher: FxBuildHasher,

    /// A shift used to determine the shard for a given hash.
    shift: u32,

    /// The key map, sharded by hash, with each shard's values for the reuse sweep behind its
    /// lock.
    shards: ShardedTable<ShardValues>,

    /// A queue of recent revisions in which values were interned.
    revision_queue: RevisionQueue,

    memo_table_types: Arc<MemoTableTypes>,

    _marker: PhantomData<fn() -> C>,
}

/// A shard's reusable values in the order they were allocated, behind the shard's lock: what
/// the CLOCK sweep walks. Its choice depends only on this and the values' own state, as the
/// LRU list's did. Immortal values are never reused and are not listed.
#[derive(Default)]
struct ShardValues {
    /// Append-only until `&mut` resets it. 16 bytes a reusable value.
    values: Vec<Member>,
    /// Where the next sweep starts.
    cursor: usize,
}

#[derive(Clone, Copy)]
struct Member {
    value: ValueKey,
    /// The hash the value's current fields are in the key map under, so reuse removes it
    /// without hashing the old fields again.
    hash: u64,
}

// SAFETY: `shard` is immutable. `id` and `last_interned_at` are atomic. `fields` and `memos` are
// written only by the holder of the shard lock after claiming the value (`InternedAt::claim`),
// which succeeds only when no reader has it pinned and it was not recorded in the current
// revision; a reader reads `fields` only holding that lock, with the value pinned, or after
// seeing it recorded in the current revision, so no read overlaps a write. `memos` supports
// concurrent shared access, and is mutated only with exclusive access or after a claim
// guarantees no shared references remain.
unsafe impl<C: Configuration> Sync for Value<C> {}

/// Struct storing the interned fields.
///
/// 24 bytes plus the fields on 64-bit targets, the memo row included (`_static_assertions`).
pub struct Value<C>
where
    C: Configuration,
{
    /// The index of the shard containing this value.
    ///
    /// This is immutable after construction and may be read to locate the protecting shard lock.
    shard: u16,

    /// The minimum durability of all inputs consumed by the creator query.
    ///
    /// Raised only under the shard lock, and only when an interning query's durability is
    /// higher. Readers may observe non-reusable durability without the lock, so on reuse it is
    /// stored only after the fields and memos are ready. A non-reusable value never becomes
    /// reusable again.
    durability: AtomicDurability,

    /// The interned ID for this value.
    ///
    /// Values that are reused increment the ID generation, as if they had
    /// allocated a new slot. This eliminates the need for dependency edges
    /// on queries that *read* from an interned value, as any memos dependent
    /// on the previous value will not match the new ID.
    ///
    /// However, reusing a slot invalidates the previous ID, so queries that
    /// *create* a reusable interned value record dependency edges to ensure the
    /// value is re-interned with a new ID.
    /// Written only by a reuser holding the claim, before it publishes `last_interned_at`.
    id: AtomicId,

    /// The revision the value was most recently interned in, and the reader protocol.
    last_interned_at: InternedAt,

    /// The interned fields for this value.
    ///
    /// These are valid for read-only access as long as the lock is held, the value is pinned,
    /// or the value has been validated in the current revision.
    fields: UnsafeCell<C::Fields<'static>>,

    /// Memos attached to this interned value.
    ///
    /// This is valid for read-only access as long as the lock is held
    /// or the value has been validated in the current revision.
    memos: UnsafeCell<MemoTable>,
}

/// A value's [`Id`], as bits.
struct AtomicId(AtomicU64);

impl AtomicId {
    fn new(id: Id) -> Self {
        Self(AtomicU64::new(id.as_bits()))
    }

    #[inline]
    fn load(&self) -> Id {
        Id::from_bits(self.0.load(Ordering::Acquire))
    }

    /// Called by a reuser holding the claim; the `Release` of `last_interned_at` publishes it.
    fn store(&self, id: Id) {
        self.0.store(id.as_bits(), Ordering::Relaxed);
    }
}

/// `Value::last_interned_at`: one word on the value's own line, holding the revision the value
/// was last interned or validated in (high 48 bits) and how many readers have it pinned (low 16).
///
/// A value recorded in the current revision is *fresh*: it can't be reused until a new revision,
/// so its fields and ID are read with no lock. A reader that finds a stale value under its hash
/// *pins* it (one CAS adding a reader) before comparing its fields, and unpins it after: when
/// they matched, recording the current revision in the same CAS. A reuser, holding the shard
/// lock, *claims* a stale value by a CAS from its stale revision with no pins to [`CLAIMED`],
/// before touching `fields`, `memos` or `id`, and publishes the new revision with `Release` once
/// they are ready. A pin and a claim are CASes on one word: exactly one wins, and a reader that
/// loses takes the locked path. A reuser never publishes a revision older than the current one,
/// so no CAS succeeds on a value that changed since its word was loaded.
///
/// So a probe that finds another key's value under a colliding hash leaves it as it was, and the
/// sweep reuses exactly the values the shard's lock-holder would see as stale.
struct InternedAt(AtomicU64);

const PIN_BITS: u32 = 16;
const PINS: u64 = (1 << PIN_BITS) - 1;
/// The revision field of `Revision::max()`: interned outside a query, never stale.
const FOREVER: u64 = u64::MAX >> PIN_BITS;
/// The revision field while a reuser holds the value. Above every real revision.
const CLAIMED: u64 = FOREVER - 1;

/// What a reader that found a value with no lock may do with its fields.
enum Seen {
    /// Recorded in the current revision: read them.
    Fresh,
    /// Pinned by this reader: read them, then unpin.
    Pinned,
    /// Claimed, saturated with pins, or another key's: leave the value alone.
    Skip,
}

impl InternedAt {
    fn new(revision: Revision) -> Self {
        Self(AtomicU64::new(Self::encode(revision) << PIN_BITS))
    }

    #[inline]
    fn encode(revision: Revision) -> u64 {
        if revision == Revision::max() {
            return FOREVER;
        }
        let field = revision.as_usize() as u64;
        assert!(
            field < CLAIMED,
            "revision {revision:?} is past what an interned value records"
        );
        field
    }

    /// The revision recorded, `None` while claimed.
    #[inline]
    fn revision(&self) -> Option<Revision> {
        match self.0.load(Ordering::Acquire) >> PIN_BITS {
            CLAIMED => None,
            FOREVER => Some(Revision::max()),
            field => Some(Revision::from(field as usize)),
        }
    }

    /// Whether the value is recorded in `current` or later. Without the shard lock, a value that
    /// isn't may be claimed at any moment.
    #[inline]
    fn is_fresh(&self, current: Revision) -> bool {
        let field = self.0.load(Ordering::Acquire) >> PIN_BITS;
        field >= Self::encode(current) && field != CLAIMED
    }

    /// Before reading the fields of a value found with no lock. `same_key` is asked only of a
    /// stale value, before pinning it.
    #[inline]
    fn pin(&self, current: Revision, same_key: impl FnOnce() -> bool) -> Seen {
        let current = Self::encode(current);
        let mut word = self.0.load(Ordering::Acquire);
        let field = word >> PIN_BITS;
        if field == CLAIMED {
            return Seen::Skip;
        }
        if field >= current {
            return Seen::Fresh;
        }
        if !same_key() {
            return Seen::Skip;
        }
        loop {
            let field = word >> PIN_BITS;
            if field == CLAIMED || word & PINS == PINS {
                return Seen::Skip;
            }
            if field >= current {
                return Seen::Fresh;
            }
            match self
                .0
                .compare_exchange(word, word + 1, Ordering::Acquire, Ordering::Acquire)
            {
                Ok(_) => return Seen::Pinned,
                Err(now) => word = now,
            }
        }
    }

    /// Drops this reader's pin; with `Some(current)`, also records `current` if older. Returns
    /// whether this call recorded it.
    fn unpin(&self, validate: Option<Revision>) -> bool {
        let Some(current) = validate else {
            // Release: the reads of the fields happen before a reuser's claim.
            self.0.fetch_sub(1, Ordering::Release);
            return false;
        };
        let current = Self::encode(current);
        let mut word = self.0.load(Ordering::Relaxed);
        loop {
            // Pinned, the value can't be claimed: its field is a revision.
            let (next, recorded) = if word >> PIN_BITS < current {
                ((current << PIN_BITS) | ((word & PINS) - 1), true)
            } else {
                (word - 1, false)
            };
            match self
                .0
                .compare_exchange(word, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return recorded,
                Err(now) => word = now,
            }
        }
    }

    /// Records `current` if older, keeping the value from reuse until a new revision. `None` if
    /// it is claimed; otherwise whether this call recorded it.
    fn raise(&self, current: Revision) -> Option<bool> {
        let current = Self::encode(current);
        let mut word = self.0.load(Ordering::Acquire);
        loop {
            let field = word >> PIN_BITS;
            if field == CLAIMED {
                return None;
            }
            if field >= current {
                return Some(false);
            }
            let next = (current << PIN_BITS) | (word & PINS);
            match self
                .0
                .compare_exchange(word, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(true),
                Err(now) => word = now,
            }
        }
    }

    /// Claims a value last recorded at `stale`, which no reader pins, for reuse. Called by the
    /// holder of the shard lock.
    fn claim(&self, stale: Revision) -> bool {
        self.0
            .compare_exchange(
                Self::encode(stale) << PIN_BITS,
                CLAIMED << PIN_BITS,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Publishes a claimed value's revision, once its fields, ID and memos are ready.
    fn publish(&self, revision: Revision) {
        self.0
            .store(Self::encode(revision) << PIN_BITS, Ordering::Release);
    }
}

/// A reader's pin on a stale value; dropped by unwinding (a panicking `Eq`), it unpins.
struct Pin<'a>(&'a InternedAt);

impl Pin<'_> {
    /// Unpins, recording `validate` if given; whether this call recorded it.
    fn release(self, validate: Option<Revision>) -> bool {
        let at = self.0;
        std::mem::forget(self);
        at.unpin(validate)
    }
}

impl Drop for Pin<'_> {
    fn drop(&mut self) {
        self.0.unpin(None);
    }
}

/// A stale value the sweep claimed.
struct Claimed {
    /// Its index in the shard's `values`.
    at: usize,
    value: ValueKey,
    old_hash: u64,
    stale: Revision,
    old_id: Id,
    new_id: Id,
}

/// Hands a claimed value back, unchanged and still stale, if reusing it unwinds before its
/// fields change.
struct Unclaim<'a> {
    at: &'a InternedAt,
    stale: Revision,
}

impl Drop for Unclaim<'_> {
    fn drop(&mut self) {
        self.at.publish(self.stale);
    }
}

/// Returns `true` if a value slot with the given durability can be reused when interning.
///
/// This intentionally remains generic so the compiler specializes the immortal and reusable
/// configurations instead of branching on `REVISIONS` at runtime.
fn is_reusable<C: Configuration>(durability: Durability) -> bool {
    // Garbage collection is disabled.
    if C::REVISIONS == IMMORTAL {
        return false;
    }

    // Collecting higher durability values requires invalidating the revision for their
    // durability (see `Database::synthetic_write`, which requires a mutable reference to
    // the database) to avoid short-circuiting calls to `maybe_changed_after`. This is
    // necessary because `maybe_changed_after` for interned values is not "pure"; it updates
    // the `last_interned_at` field before validating a given value to ensure that it is not
    // reused after read in the current revision.
    durability == Durability::LOW
}

/// Record a dependency on this value if its slot can be reused.
fn report_tracked_read_if_reusable<C: Configuration>(
    zalsa_local: &ZalsaLocal,
    index: DatabaseKeyIndex,
    current_revision: Revision,
    durability: Durability,
) {
    if is_reusable::<C>(durability) {
        zalsa_local.report_tracked_read_simple(index, durability, current_revision);
    } else {
        // The value cannot be reused, so the dependency edge is unnecessary. Its durability
        // is derived from the active query, but its revision must still contribute to the
        // query's stamp because the interned ID may have changed due to reuse.
        zalsa_local.report_tracked_read_revision(current_revision);
    }
}

impl<C> Value<C>
where
    C: Configuration,
{
    /// Fields of this interned struct.
    #[cfg(feature = "salsa_unstable")]
    pub fn fields<'db>(&'db self) -> &'db C::Fields<'db> {
        // SAFETY: The fact that this function is safe is technically unsound. However, interned
        // values are only exposed if they have been validated in the current revision, which
        // ensures that they are not reused while being accessed.
        // SAFETY: Guaranteed by `Configuration`; the restored lifetime is
        // bounded by the borrow of this interned value.
        unsafe { std::mem::transmute::<&C::Fields<'static>, &C::Fields<'db>>(&*self.fields.get()) }
    }

    /// Returns memory usage information about the interned value.
    ///
    /// # Safety
    ///
    /// The `MemoTable` must belong to a `Value` of the correct type. The caller must
    /// ensure exclusive access to the database's storage for the duration of this call.
    #[cfg(all(not(feature = "shuttle"), feature = "salsa_unstable"))]
    unsafe fn memory_usage(&self, memo_table_types: &MemoTableTypes) -> crate::database::SlotInfo {
        let heap_size = C::heap_size(self.fields());
        // SAFETY: The caller guarantees exclusive access to the database's storage,
        // so the value's memos cannot be modified while we read them.
        let memos = unsafe { &*self.memos.get() };
        // SAFETY: The caller guarantees this is the correct types table.
        let memos = unsafe { memo_table_types.attach_memos(memos) };

        crate::database::SlotInfo {
            debug_name: C::DEBUG_NAME,
            size_of_metadata: std::mem::size_of::<Self>()
                - std::mem::size_of::<C::Fields<'static>>(),
            size_of_fields: std::mem::size_of::<C::Fields<'static>>(),
            heap_size_of_fields: heap_size,
            // SAFETY: The caller guarantees exclusive access to the database's storage.
            memos: unsafe { memos.memory_usage() },
        }
    }
}

impl<C: Configuration> Default for JarImpl<C> {
    fn default() -> Self {
        Self {
            phantom: PhantomData,
        }
    }
}

impl<C: Configuration> Jar for JarImpl<C> {
    fn create_ingredients(
        _zalsa: &mut Zalsa,
        first_index: IngredientIndex,
    ) -> Vec<Box<dyn Ingredient>> {
        vec![Box::new(IngredientImpl::<C>::new(first_index)) as _]
    }

    fn id_struct_type_id() -> TypeId {
        TypeId::of::<C::Struct<'static>>()
    }
}

impl<C> IngredientImpl<C>
where
    C: Configuration,
{
    pub fn new(ingredient_index: IngredientIndex) -> Self {
        let shards = new_shards();
        let shift = usize::BITS - shards.shards().trailing_zeros();

        Self {
            ingredient_index,
            hasher: FxBuildHasher,
            memo_table_types: Arc::new(MemoTableTypes::default()),
            revision_queue: RevisionQueue::new(C::REVISIONS),
            shift,
            shards,
            _marker: PhantomData,
        }
    }
    /// Returns the shard for a given hash.
    ///
    /// Note that this value is guaranteed to be in-bounds for `self.shards`. The key map probes
    /// from the hash's low bits and tags with its top seven; the shard takes the bits between.
    #[inline]
    fn shard(&self, hash: u64) -> usize {
        // https://github.com/xacrimon/dashmap/blob/366ce7e7872866a06de66eb95002fa6cf2c117a7/src/lib.rs#L421
        ((hash as usize) << 7) >> self.shift
    }

    /// # Safety
    ///
    /// The `from_internal_data` function must be called to restore the correct lifetime
    /// before access.
    unsafe fn to_internal_data<'db>(&'db self, data: C::Fields<'db>) -> C::Fields<'static> {
        // SAFETY: Guaranteed by `Configuration` and retained in this ingredient.
        unsafe { std::mem::transmute::<C::Fields<'db>, C::Fields<'static>>(data) }
    }

    fn from_internal_data<'db>(data: &'db C::Fields<'static>) -> &'db C::Fields<'db> {
        // SAFETY: Guaranteed by `Configuration`; the restored lifetime is
        // bounded by the borrow of the retained data.
        unsafe { std::mem::transmute::<&'db C::Fields<'static>, &'db C::Fields<'db>>(data) }
    }

    /// Intern data to a unique reference.
    ///
    /// If `key` is already interned, returns the existing [`Id`] for the interned data.
    pub fn intern<'db, Key>(
        &'db self,
        zalsa: &'db Zalsa,
        zalsa_local: &'db ZalsaLocal,
        key: Key,
    ) -> C::Struct<'db>
    where
        Key: Hash,
        C::Fields<'db>: HashEqLike<Key>,
        Key: Lookup<C::Fields<'db>>,
    {
        FromId::from_id(self.intern_id(zalsa, zalsa_local, key))
    }

    /// Intern data to a unique reference.
    ///
    /// If `key` is already interned, returns the existing [`Id`] for the interned data.
    pub fn intern_id<'db, Key>(
        &'db self,
        zalsa: &'db Zalsa,
        zalsa_local: &'db ZalsaLocal,
        key: Key,
    ) -> crate::Id
    where
        Key: Hash,
        // We'd want the following predicate, but this currently implies `'static` due to a rustc
        // bug
        // for<'db> C::Data<'db>: HashEqLike<Key>,
        // so instead we go with this and transmute the lifetime in the `eq` closure
        C::Fields<'db>: HashEqLike<Key>,
        Key: Lookup<C::Fields<'db>>,
    {
        // Record the current revision as active.
        let current_revision = zalsa.current_revision();
        if C::REVISIONS != IMMORTAL {
            self.revision_queue.record(current_revision);
        }

        let hash = self.hasher.hash_one(&key);
        let shard_index = self.shard(hash);

        // Look with no lock. A fresh value is compared directly; a stale one is pinned first
        // (`InternedAt`), and only if it was inserted under this very hash.
        let mut validated = false;
        let found = self.shards.find(shard_index, hash, |candidate| {
            // SAFETY: Values in the key map are allocated in the database's table, which outlives
            // the ingredient's `&self` borrow.
            let value = unsafe { ValueKey(candidate.value()).value::<C>() };
            if C::REVISIONS == IMMORTAL {
                // SAFETY: An immortal value is never reused: its fields were written once, before
                // the key map published it to us.
                return unsafe { Self::value_eq(value, &key) };
            }
            match value
                .last_interned_at
                .pin(current_revision, || candidate.hash() == hash)
            {
                Seen::Fresh => {
                    // SAFETY: Recorded in the current revision, the value can't be claimed, so its
                    // fields aren't written, until a new revision.
                    unsafe { Self::value_eq(value, &key) }
                }
                Seen::Pinned => {
                    let pin = Pin(&value.last_interned_at);
                    // SAFETY: Pinned, the value can't be claimed, so its fields aren't written,
                    // until the pin is released.
                    let eq = unsafe { Self::value_eq(value, &key) };
                    validated = pin.release(eq.then_some(current_revision));
                    eq
                }
                Seen::Skip => false,
            }
        });
        if let Some(found) = found {
            // SAFETY: As above.
            let value = unsafe { ValueKey(found).value::<C>() };
            return self.hit(zalsa, zalsa_local, value, validated, current_revision, false);
        }

        self.intern_locked(zalsa, zalsa_local, key, hash, shard_index, current_revision)
    }

    /// Finishes a hit on `value`: the event if this call recorded it in the current revision,
    /// its durability raised to the active query's, and the dependency.
    #[inline]
    fn hit(
        &self,
        zalsa: &Zalsa,
        zalsa_local: &ZalsaLocal,
        value: &Value<C>,
        validated: bool,
        current_revision: Revision,
        locked: bool,
    ) -> Id {
        // Found fresh, pinned and then recorded, or under the lock: the ID can't change until a
        // new revision.
        let id = value.id.load();
        let index = self.database_key_index(id);

        if validated {
            zalsa.event(&|| {
                Event::new(EventKind::DidValidateInternedValue {
                    key: index,
                    revision: current_revision,
                })
            });
        }

        let mut durability = value.durability.load();
        if let Some((_, stamp)) = zalsa_local.active_query()
            && stamp.durability > durability
        {
            // Record the maximum durability across all queries that intern this value.
            durability = self.raise_durability(value, stamp.durability, locked);
        }

        // Record a dependency on the value if its slot can be reused.
        //
        // See `allocate` for why we need to use `current_revision` here. Note that just
        // because this value was previously interned does not mean it was previously interned
        // by *our query*, so the same considerations apply.
        report_tracked_read_if_reusable::<C>(zalsa_local, index, current_revision, durability);

        id
    }

    /// Raises `value`'s durability to at least `to`, under its shard lock so no raise is lost.
    #[cold]
    fn raise_durability(&self, value: &Value<C>, to: Durability, locked: bool) -> Durability {
        let _shard = (!locked).then(|| self.shards.lock(value.shard as usize));
        let durability = std::cmp::max(value.durability.load(), to);
        value.durability.store(durability);
        durability
    }

    /// A miss with no lock, or a value another thread held: look again under the shard lock,
    /// then reuse a stale value or allocate one.
    #[inline(never)]
    fn intern_locked<'db, Key>(
        &'db self,
        zalsa: &'db Zalsa,
        zalsa_local: &'db ZalsaLocal,
        key: Key,
        hash: u64,
        shard_index: usize,
        current_revision: Revision,
    ) -> crate::Id
    where
        Key: Hash,
        C::Fields<'db>: HashEqLike<Key>,
        Key: Lookup<C::Fields<'db>>,
    {
        let mut shard = self.shards.lock(shard_index);

        let found = shard.find(hash, |candidate| {
            // SAFETY: Values in the key map live as long as the database's table.
            let value = unsafe { ValueKey(candidate.value()).value::<C>() };
            // SAFETY: We hold the shard lock, and only its holder writes its values' fields.
            unsafe { Self::value_eq(value, &key) }
        });
        if let Some(found) = found {
            // SAFETY: As above.
            let value = unsafe { ValueKey(found).value::<C>() };
            // Under the lock nothing in the shard is claimed: a reuse completes or hands the
            // value back before unlocking.
            let validated = C::REVISIONS != IMMORTAL
                && value
                    .last_interned_at
                    .raise(current_revision)
                    .expect("a value in the key map is claimed only under its shard lock");
            return self.hit(zalsa, zalsa_local, value, validated, current_revision, true);
        }

        // Assemble the fields before changing anything: it can invoke user code and panic.
        // SAFETY: We call `from_internal_data` to restore the correct lifetime before access.
        let fields = unsafe { self.to_internal_data(key.into_owned()) };

        // Record the durability of the current query on the interned value.
        let (durability, last_interned_at) = zalsa_local
            .active_query()
            .map(|(_, stamp)| (stamp.durability, current_revision))
            // If there is no active query this durability does not actually matter.
            // `last_interned_at` needs to be `Revision::MAX`, see the `intern_access_in_different_revision` test.
            .unwrap_or((Durability::MAX, Revision::max()));

        // Fill up the table for the first few revisions without attempting garbage collection.
        if C::REVISIONS != IMMORTAL
            && self.revision_queue.is_primed()
            && let Some(claimed) = self.claim_stale(&mut shard, current_revision)
        {
            return self.reuse(
                zalsa,
                zalsa_local,
                shard,
                claimed,
                fields,
                hash,
                durability,
                last_interned_at,
                current_revision,
            );
        }

        self.allocate(
            zalsa,
            zalsa_local,
            shard,
            shard_index,
            fields,
            hash,
            durability,
            last_interned_at,
            current_revision,
        )
    }

    /// The CLOCK sweep: looks at up to [`SWEEP`] of the shard's values from its cursor for one
    /// that is reusable, stale and unpinned, and claims it.
    fn claim_stale(
        &self,
        shard: &mut Locked<'_, ShardValues>,
        current_revision: Revision,
    ) -> Option<Claimed> {
        let state = shard.state_mut();
        let len = state.values.len();
        for _ in 0..len.min(SWEEP) {
            let at = state.cursor;
            state.cursor = if at + 1 == len { 0 } else { at + 1 };
            let member = state.values[at];
            // SAFETY: Listed values live as long as the database's table.
            let value = unsafe { member.value.value::<C>() };

            if !is_reusable::<C>(value.durability.load()) {
                continue;
            }
            // The value must not have been read in the current revision to be collected
            // soundly, but we also do not want to collect values that have been read recently.
            let Some(last_interned_at) = value.last_interned_at.revision() else {
                continue;
            };
            if !self.revision_queue.is_stale(last_interned_at) {
                continue;
            }

            // We should never reuse a value that was accessed in the current revision.
            assert!(last_interned_at < current_revision);

            // Increment the generation of the ID, as if we allocated a new slot.
            //
            // If the ID is at its maximum generation, we are forced to leak the slot.
            let old_id = value.id.load();
            let Some(new_id) = old_id.next_generation() else {
                continue;
            };

            // Fails if a reader pinned or recorded it since.
            if !value.last_interned_at.claim(last_interned_at) {
                continue;
            }

            return Some(Claimed {
                at,
                value: member.value,
                old_hash: member.hash,
                stale: last_interned_at,
                old_id,
                new_id,
            });
        }
        None
    }

    /// Reuses a claimed value for `fields`.
    #[allow(clippy::too_many_arguments)]
    fn reuse(
        &self,
        zalsa: &Zalsa,
        zalsa_local: &ZalsaLocal,
        mut shard: Locked<'_, ShardValues>,
        claimed: Claimed,
        fields: C::Fields<'static>,
        hash: u64,
        durability: Durability,
        last_interned_at: Revision,
        current_revision: Revision,
    ) -> Id {
        // SAFETY: Listed values live as long as the database's table.
        let value = unsafe { claimed.value.value::<C>() };
        let unclaim = Unclaim {
            at: &value.last_interned_at,
            stale: claimed.stale,
        };

        let index = self.database_key_index(claimed.new_id);

        // Record a dependency on the new value if its slot can be reused.
        //
        // See `allocate` for why we need to use `current_revision` here.
        report_tracked_read_if_reusable::<C>(zalsa_local, index, current_revision, durability);

        // Clear the old memos first. If a callback panics, `unclaim` hands the value back with
        // its old fields, ID and key-map entry; `clear_memos` drops the remaining memos during
        // unwinding.
        // SAFETY: The value is claimed and we hold its shard lock, so no reference to its memos
        // remains: it was stale, so nothing validated it in this revision.
        unsafe { self.clear_memos(zalsa, &mut *value.memos.get(), claimed.old_id) };
        std::mem::forget(unclaim);

        // The old key leaves the map. The old and new hashes map to the same shard, because the
        // sweep ran in the new key's shard.
        let removed = shard.remove(claimed.old_hash, claimed.value.0);
        assert!(removed, "a value listed in its shard must be in its key map");

        // Replace the fields without dropping the previous value until the slot is consistent.
        //
        // SAFETY: The value is claimed and we hold its shard lock: no reader reads its fields
        // until `publish` below, and none holds a reference from an earlier revision.
        let old_fields = unsafe { std::mem::replace(&mut *value.fields.get(), fields) };
        value.id.store(claimed.new_id);
        shard.insert(hash, claimed.value.0);
        shard.state_mut().values[claimed.at].hash = hash;

        // Either atomic can allow a reader to access the slot without locking. Publish only
        // after the new fields, ID and empty memo table are ready. Doing this before dropping
        // the old fields also leaves a valid slot if their destructor panics.
        value.durability.store(durability);
        value.last_interned_at.publish(last_interned_at);
        drop(shard);

        drop(old_fields);

        zalsa.event(&|| {
            Event::new(EventKind::DidReuseInternedValue {
                key: index,
                revision: current_revision,
            })
        });

        claimed.new_id
    }

    /// Allocates a new slot for `fields`.
    #[allow(clippy::too_many_arguments)]
    fn allocate(
        &self,
        zalsa: &Zalsa,
        zalsa_local: &ZalsaLocal,
        mut shard: Locked<'_, ShardValues>,
        shard_index: usize,
        fields: C::Fields<'static>,
        hash: u64,
        durability: Durability,
        last_interned_at: Revision,
        current_revision: Revision,
    ) -> Id {
        let (id, value) = zalsa_local.allocate(zalsa, self.ingredient_index, |id| Value::<C> {
            shard: shard_index as u16,
            durability: AtomicDurability::new(durability),
            id: AtomicId::new(id),
            last_interned_at: InternedAt::new(last_interned_at),
            fields: UnsafeCell::new(fields),
            // SAFETY: We only ever access the memos of a value that we allocated through
            // our `MemoTableTypes`.
            memos: UnsafeCell::new(unsafe { MemoTable::new(self.memo_table_types()) }),
        });

        // SAFETY: We hold this ingredient's shard lock, and `value` is the live, stable value that
        // we just allocated in that shard.
        unsafe { Self::insert_value(&mut shard, hash, value) };
        drop(shard);

        let index = self.database_key_index(id);

        // Record a dependency on the newly interned value if its slot can be reused.
        //
        // Note that the ID is unique to this use of the interned slot, so it seems logical to use
        // `Revision::start()` here. However, it is possible that the ID we read is different from
        // the previous execution of this query if the previous slot has been reused. In that case,
        // the query has changed without a corresponding input changing. Using `current_revision`
        // for dependencies on interned values encodes the fact that interned IDs are not stable
        // across revisions.
        report_tracked_read_if_reusable::<C>(zalsa_local, index, current_revision, durability);

        zalsa.event(&|| {
            Event::new(EventKind::DidInternValue {
                key: index,
                revision: current_revision,
            })
        });

        id
    }

    /// Publishes a newly allocated value in the key map and, if it can ever be reused, lists it
    /// for the sweep.
    ///
    /// # Safety
    ///
    /// `shard` must be a locked shard from this ingredient, and `value` must be the live, stable
    /// `Value<C>` allocated in that shard, its fields hashing to `hash`.
    unsafe fn insert_value(shard: &mut Locked<'_, ShardValues>, hash: u64, value: &Value<C>) {
        let value = ValueKey::new(value);
        if C::REVISIONS != IMMORTAL {
            shard.state_mut().values.push(Member { value, hash });
        }
        shard.insert(hash, value.0);
    }

    /// Clears the given memo table.
    ///
    /// # Safety
    ///
    /// `memo_table` must belong to the value identified by `id` in this ingredient. The caller
    /// must have exclusive access to the table.
    pub(crate) unsafe fn clear_memos(&self, zalsa: &Zalsa, memo_table: &mut MemoTable, id: Id) {
        /// Clears the given memo table without depending on `C`.
        ///
        /// # Safety
        ///
        /// `memo_table` must belong to the value identified by `id` in `ingredient_index` and must
        /// have been created with `memo_table_types`. The caller must have exclusive access to the
        /// table.
        unsafe fn inner(
            zalsa: &Zalsa,
            ingredient_index: IngredientIndex,
            memo_table_types: &MemoTableTypes,
            memo_table: &mut MemoTable,
            id: Id,
        ) {
            // SAFETY: The caller guarantees this is the correct types table.
            let table = unsafe { memo_table_types.attach_memos_mut(memo_table) };

            // `Database::salsa_event` is a user supplied callback which may panic
            // in that case we need a drop guard to free the memo table
            struct TableDropGuard<'a>(MemoTableWithTypesMut<'a>);

            impl Drop for TableDropGuard<'_> {
                fn drop(&mut self) {
                    // SAFETY: We have `&mut MemoTable`, so no more references to these memos exist and we are good
                    // to drop them.
                    unsafe { self.0.drop() };
                }
            }

            let mut table_guard = TableDropGuard(table);

            // SAFETY: We have `&mut MemoTable`, so nobody else reaches these memos through the slot;
            // retired memos stay valid until the next reclaim, so outstanding borrows remain sound.
            unsafe {
                table_guard.0.take_memos(|memo_ingredient_index, memo| {
                    let ingredient_index =
                        zalsa.ingredient_index_for_memo(ingredient_index, memo_ingredient_index);

                    let executor = DatabaseKeyIndex::new(ingredient_index, id);

                    zalsa.event(&|| Event::new(EventKind::DidDiscard { key: executor }));

                    memo.remove_outputs(zalsa, executor);
                })
            };

            std::mem::forget(table_guard);

            // Reset the table after having dropped any memos.
            memo_table.reset();
        }

        // SAFETY: The caller guarantees the table belongs to this ingredient's value, so these are
        // the correct ingredient index and memo-table types.
        unsafe {
            inner(
                zalsa,
                self.ingredient_index,
                &self.memo_table_types,
                memo_table,
                id,
            )
        }
    }

    // Compares the value by its fields to the given key.
    //
    // # Safety
    //
    // No writer may write the value's fields during the call: the caller holds the shard lock,
    // has the value pinned, or saw it recorded in the current revision.
    unsafe fn value_eq<'db, Key>(value: &'db Value<C>, key: &Key) -> bool
    where
        C::Fields<'db>: HashEqLike<Key>,
    {
        // SAFETY: Guaranteed by the caller.
        let fields = unsafe { &*value.fields.get() };

        HashEqLike::eq(Self::from_internal_data(fields), key)
    }

    /// Returns the database key index for an interned value with the given id.
    #[inline]
    pub fn database_key_index(&self, id: Id) -> DatabaseKeyIndex {
        DatabaseKeyIndex::new(self.ingredient_index, id)
    }

    /// Lookup the data for an interned value based on its ID.
    #[inline]
    pub fn data<'db>(&'db self, zalsa: &'db Zalsa, id: Id) -> &'db C::Fields<'db> {
        let value = zalsa.table().get::<Value<C>>(id);
        value.assert_validated(zalsa.current_revision());

        debug_assert_eq!(
            id,
            value.id.load(),
            "Interned ID for `{}` refers to a reused slot",
            C::DEBUG_NAME,
        );

        // SAFETY: Reusable interned values are only exposed if they have been validated
        // in the current revision, as checked by the assertion above, which ensures that
        // they are not reused while being accessed. Non-reusable values are never reused.
        unsafe { Self::from_internal_data(&*value.fields.get()) }
    }

    /// Lookup the fields from an interned struct.
    ///
    /// Note that this is not "leaking" since no dependency edge is required.
    #[inline]
    pub fn fields<'db>(&'db self, zalsa: &'db Zalsa, s: C::Struct<'db>) -> &'db C::Fields<'db> {
        self.data(zalsa, AsId::as_id(&s))
    }

    pub fn reset(&mut self, zalsa_mut: &mut Zalsa) {
        _ = zalsa_mut;

        // We can clear the key maps and sweep lists: `zalsa_mut` means no query runs.
        self.shards
            .clear(|values| *values = ShardValues::default());
    }

    /// Returns all data corresponding to the interned struct.
    pub fn entries<'db>(&'db self, zalsa: &'db Zalsa) -> impl Iterator<Item = StructEntry<'db, C>> {
        // SAFETY: `should_lock` is `true`
        unsafe { self.entries_inner(true, zalsa) }
    }

    /// Returns all data corresponding to the interned struct.
    ///
    /// # Safety
    ///
    /// If `should_lock` is `false`, the caller must hold the locks for all shards
    /// of the key map or ensure exclusive access to the database's storage while
    /// consuming the iterator.
    unsafe fn entries_inner<'db>(
        &'db self,
        should_lock: bool,
        zalsa: &'db Zalsa,
    ) -> impl Iterator<Item = StructEntry<'db, C>> {
        zalsa.table().slots_of::<Value<C>>().map(move |(_, value)| {
            let id = if should_lock {
                let _shard = self.shards.lock(value.shard as usize);

                // Entries expose borrowed fields, so keep the slot alive for this revision.
                // Preserve the maximum revision used by values interned outside queries. This
                // marks entries as interned in this revision, so enumeration also counts as a
                // use for reuse. A value left claimed by an aborted reuse is no longer in the
                // key map and stays as it is.
                let _ = value.last_interned_at.raise(zalsa.current_revision());
                value.id.load()
            } else {
                value.id.load()
            };

            StructEntry {
                value,
                key: self.database_key_index(id),
            }
        })
    }
}

/// Creates the sharded storage outside of the generic [`IngredientImpl::new`] context.
///
/// Keeping this helper non-generic avoids monomorphizing the iterator machinery for every interned
/// struct configuration.
fn new_shards() -> ShardedTable<ShardValues> {
    let shard_count = (max_parallelism() * 4).next_power_of_two();
    ShardedTable::new(shard_count, ShardValues::default)
}

/// A stable pointer to an interned value allocated in the database table.
#[repr(transparent)]
#[derive(Clone, Copy)]
struct ValueKey(NonNull<()>);

impl ValueKey {
    fn new<C: Configuration>(value: &Value<C>) -> Self {
        Self(NonNull::from(value).cast())
    }

    /// # Safety
    ///
    /// The database table containing this value must still be alive.
    unsafe fn value<'db, C: Configuration>(&self) -> &'db Value<C> {
        // SAFETY: Guaranteed by the caller. The erased pointer was created from a `Value<C>`.
        unsafe { &*self.0.as_ptr().cast::<Value<C>>() }
    }
}

// SAFETY: Values remain allocated until after the ingredient and its key map are dropped.
// Access to their mutable state is synchronized by `InternedAt` and the shard lock.
unsafe impl Send for ValueKey {}
// SAFETY: See the `Send` implementation above.
unsafe impl Sync for ValueKey {}

/// An interned struct entry.
pub struct StructEntry<'db, C>
where
    C: Configuration,
{
    #[allow(dead_code)]
    value: &'db Value<C>,
    key: DatabaseKeyIndex,
}

impl<'db, C> StructEntry<'db, C>
where
    C: Configuration,
{
    /// Returns the `DatabaseKeyIndex` for this entry.
    pub fn key(&self) -> DatabaseKeyIndex {
        self.key
    }

    /// Returns the interned struct.
    pub fn as_struct(&self) -> C::Struct<'_> {
        FromId::from_id(self.key.key_index())
    }

    #[cfg(feature = "salsa_unstable")]
    pub fn value(&self) -> &'db Value<C> {
        self.value
    }
}

impl<C> Ingredient for IngredientImpl<C>
where
    C: Configuration,
{
    fn location(&self) -> &'static crate::ingredient::Location {
        &C::LOCATION
    }

    fn ingredient_index(&self) -> IngredientIndex {
        self.ingredient_index
    }

    unsafe fn maybe_changed_after(
        &self,
        zalsa: &crate::zalsa::Zalsa,
        _db: crate::database::RawDatabase<'_>,
        input: Id,
        _revision: Revision,
    ) -> VerifyResult {
        // Record the current revision as active.
        let current_revision = zalsa.current_revision();
        if C::REVISIONS != IMMORTAL {
            self.revision_queue.record(current_revision);
        }

        let value = zalsa.table().get::<Value<C>>(input);

        // A value already validated in this revision is the common case: every memo that read
        // it asks again once per revision, from as many threads as verify memos. Its slot can't
        // be reused before the next revision, so its ID is read with no lock and nothing is
        // stored to the line the other threads are reading. A stale one is recorded by one CAS
        // on its own word, unless a reuser holds it.
        if C::REVISIONS != IMMORTAL && !value.last_interned_at.is_fresh(current_revision) {
            // The slot was reused: no need to keep it.
            if value.id.load().generation() > input.generation() {
                return VerifyResult::changed();
            }

            // Validate the value for the current revision to avoid reuse.
            if value.last_interned_at.raise(current_revision).is_none() {
                return self.maybe_changed_after_locked(zalsa, value, input, current_revision);
            }
        }

        // Recorded now (by us, a reader or a reuser), the ID can't change until a new revision.
        if value.id.load().generation() > input.generation() {
            return VerifyResult::changed();
        }

        zalsa.event(&|| {
            Event::new(EventKind::DidValidateInternedValue {
                key: self.database_key_index(input),
                revision: current_revision,
            })
        });

        // Any change to an interned value results in a new ID generation.
        VerifyResult::unchanged()
    }

    unsafe fn collect_minimum_serialized_edges(
        &self,
        _zalsa: &Zalsa,
        edge: QueryEdge,
        serialized_edges: &mut FxIndexSet<QueryEdge>,
        _visited_edges: &mut FxHashSet<QueryEdge>,
    ) {
        if C::PERSIST && C::REVISIONS != IMMORTAL {
            // If the interned struct is being persisted, it may be reachable through transitive queries.
            // Additionally, interned struct dependencies are impure in that garbage collection can
            // invalidate a dependency without a base input necessarily being updated. Thus, we must
            // preserve the transitive dependency on the interned struct, if garbage collection is
            // enabled.
            serialized_edges.insert(edge);
        }

        // Otherwise, the dependency is covered by the base inputs.
    }

    fn flatten_cycle_head_dependencies(
        &self,
        _zalsa: &Zalsa,
        id: Id,
        flattened_input_outputs: &mut FxIndexSet<QueryEdge>,
        _seen: &mut FxHashSet<DatabaseKeyIndex>,
    ) {
        flattened_input_outputs.insert(QueryEdge::input(self.database_key_index(id)));
    }

    fn debug_name(&self) -> &'static str {
        C::DEBUG_NAME
    }

    fn jar_kind(&self) -> JarKind {
        JarKind::Struct
    }

    /// Outgrown key-map tables are freed once per revision, when no reader can be probing one.
    fn requires_reset_for_new_revision(&self) -> bool {
        true
    }

    fn reset_for_new_revision(&mut self, _table: &mut crate::table::Table) {
        self.shards.free_outgrown();
    }

    unsafe fn reclaim_memos(&self) {
        // SAFETY: Our caller's contract.
        unsafe { self.memo_table_types.reclaim() };
    }

    fn memo_table_types(&self) -> &Arc<MemoTableTypes> {
        &self.memo_table_types
    }

    fn memo_table_types_mut(&mut self) -> &mut Arc<MemoTableTypes> {
        &mut self.memo_table_types
    }

    /// Returns memory usage information about any interned values.
    #[cfg(all(not(feature = "shuttle"), feature = "salsa_unstable"))]
    unsafe fn memory_usage(
        &self,
        db: &dyn crate::Database,
    ) -> Option<Vec<crate::database::SlotInfo>> {
        // SAFETY: The caller guarantees exclusive access to the database's storage.
        let entries = unsafe { self.entries_inner(false, db.zalsa()) };

        let memory_usage = entries
            // SAFETY: The memo table belongs to a value that we allocated, so it
            // has the correct type. The caller guarantees exclusive database access.
            .map(|entry| unsafe { entry.value.memory_usage(&self.memo_table_types) })
            .collect();

        Some(memory_usage)
    }

    fn is_persistable(&self) -> bool {
        C::PERSIST
    }

    fn should_serialize(&self, zalsa: &Zalsa) -> bool {
        C::PERSIST && zalsa.table().slots_of::<Value<C>>().next().is_some()
    }

    #[cfg(feature = "persistence")]
    unsafe fn serialize<'db>(
        &'db self,
        zalsa: &'db Zalsa,
        f: &mut dyn FnMut(&dyn erased_serde::Serialize),
    ) {
        f(&persistence::SerializeIngredient {
            zalsa,
            _ingredient: self,
        })
    }

    #[cfg(feature = "persistence")]
    fn deserialize(
        &mut self,
        zalsa: &mut Zalsa,
        deserializer: &mut dyn erased_serde::Deserializer,
    ) -> Result<(), erased_serde::Error> {
        let deserialize = persistence::DeserializeIngredient {
            zalsa,
            ingredient: self,
        };

        serde::de::DeserializeSeed::deserialize(deserialize, deserializer)
    }
}

impl<C> IngredientImpl<C>
where
    C: Configuration,
{
    /// `maybe_changed_after` for a value a reuser holds: wait for it under the shard lock.
    #[cold]
    fn maybe_changed_after_locked(
        &self,
        zalsa: &Zalsa,
        value: &Value<C>,
        input: Id,
        current_revision: Revision,
    ) -> VerifyResult {
        let _shard = self.shards.lock(value.shard as usize);

        // Still claimed under the lock: a reuse that unwound out of memory left it behind.
        if value.last_interned_at.raise(current_revision).is_none() {
            return VerifyResult::changed();
        }

        // The slot was reused.
        if value.id.load().generation() > input.generation() {
            return VerifyResult::changed();
        }

        zalsa.event(&|| {
            Event::new(EventKind::DidValidateInternedValue {
                key: self.database_key_index(input),
                revision: current_revision,
            })
        });

        // Any change to an interned value results in a new ID generation.
        VerifyResult::unchanged()
    }
}

impl<C> std::fmt::Debug for IngredientImpl<C>
where
    C: Configuration,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(std::any::type_name::<Self>())
            .field("index", &self.ingredient_index)
            .finish()
    }
}

// SAFETY: `Value<C>` is our private type branded over the unique configuration `C`.
unsafe impl<C> Slot for Value<C>
where
    C: Configuration,
{
    #[inline(always)]
    unsafe fn memos(
        this: *const Self,
        current_revision: Revision,
    ) -> *const crate::table::memo::MemoTable {
        // SAFETY: The caller provides a valid slot pointer and its database's current revision.
        unsafe { (*this).assert_validated(current_revision) };
        // SAFETY: The assertion ensures the slot cannot be reused while its memos are borrowed.
        unsafe { Self::memos_unchecked(this) }
    }

    #[inline(always)]
    unsafe fn memos_unchecked(this: *const Self) -> *const crate::table::memo::MemoTable {
        // SAFETY: The caller provides an initialized slot and prevents reuse.
        unsafe { (*this).memos.get() }
    }

    #[inline(always)]
    fn memos_mut(&mut self) -> &mut crate::table::memo::MemoTable {
        self.memos.get_mut()
    }
}

impl<C> Value<C>
where
    C: Configuration,
{
    /// Assert that this slot cannot be reused while its fields or memos are borrowed.
    #[inline]
    fn assert_validated(&self, current_revision: Revision) {
        // Either acquire load independently establishes that initialization and memo cleanup
        // finished: a current revision prevents reuse until the next revision, and a
        // non-reusable durability prevents reuse permanently. No atomic snapshot is needed.
        assert!(
            C::REVISIONS == IMMORTAL
                || self.last_interned_at.is_fresh(current_revision)
                || !is_reusable::<C>(self.durability.load()),
            "Data for reusable `{}` was not interned in the latest revision for its durability.",
            C::DEBUG_NAME,
        );
    }
}

/// Keep track of revisions in which interned values were read, to determine staleness.
///
/// An interned value is considered stale if it has not been read in the past `REVS`
/// revisions. However, we only consider revisions in which interned values were actually
/// read, as revisions may be created in bursts.
struct RevisionQueue {
    lock: Mutex<()>,
    /// Recent revisions, stored inline for the default configuration.
    revisions: SmallVec<[AtomicRevision; DEFAULT_REVISIONS]>,
}

// `#[salsa::interned(revisions = usize::MAX)]` disables garbage collection.
const IMMORTAL: NonZeroUsize = NonZeroUsize::MAX;

impl RevisionQueue {
    fn new(capacity: NonZeroUsize) -> Self {
        let revisions = if capacity == IMMORTAL {
            SmallVec::new()
        } else {
            (0..capacity.get())
                .map(|_| AtomicRevision::start())
                .collect()
        };

        Self {
            lock: Mutex::new(()),
            revisions,
        }
    }

    /// Record the given revision as active.
    #[inline]
    fn record(&self, revision: Revision) {
        debug_assert!(
            !self.revisions.is_empty(),
            "cannot record revisions when interned garbage collection is disabled"
        );

        // Fast-path: We already recorded this revision.
        if self.revisions[0].load() >= revision {
            return;
        }

        self.record_cold(revision);
    }

    #[cold]
    fn record_cold(&self, revision: Revision) {
        let _lock = self.lock.lock();

        // Another thread may have recorded this revision while we waited for the lock.
        if self.revisions[0].load() >= revision {
            return;
        }

        // Otherwise, update the queue, maintaining sorted order.
        //
        // Note that this should only happen once per revision.
        for i in (1..self.revisions.len()).rev() {
            self.revisions[i].store(self.revisions[i - 1].load());
        }

        self.revisions[0].store(revision);
    }

    /// Returns `true` if the given revision is old enough to be considered stale.
    #[inline]
    fn is_stale(&self, revision: Revision) -> bool {
        let Some(oldest) = self.revisions.last() else {
            return false;
        };
        let oldest = oldest.load();

        // If we have not recorded `REVS` revisions yet, nothing can be stale.
        if oldest == Revision::start() {
            return false;
        }

        revision < oldest
    }

    /// Returns `true` if the configured number of revisions have been recorded as active,
    /// i.e. enough data has been recorded to start garbage collection.
    #[inline]
    fn is_primed(&self) -> bool {
        self.revisions
            .last()
            .is_some_and(|oldest| oldest.load() > Revision::start())
    }
}

/// A trait for types that hash and compare like `O`.
pub trait HashEqLike<O> {
    fn eq(&self, data: &O) -> bool;
}

/// The `Lookup` trait is a more flexible variant on [`std::borrow::Borrow`]
/// and [`std::borrow::ToOwned`].
///
/// It is implemented by "some type that can be used as the lookup key for `O`".
/// This means that `self` can be hashed and compared for equality with values
/// of type `O` without actually creating an owned value. It `self` needs to be interned,
/// it can be converted into an equivalent value of type `O`.
///
/// The canonical example is `&str: Lookup<String>`. However, this example
/// alone can be handled by [`std::borrow::Borrow`][]. In our case, we may have
/// multiple keys accumulated into a struct, like `ViewStruct: Lookup<(K1, ...)>`,
/// where `struct ViewStruct<L1: Lookup<K1>...>(K1...)`. The `Borrow` trait
/// requires that `&(K1...)` be convertible to `&ViewStruct` which just isn't
/// possible. `Lookup` instead offers direct `hash` and `eq` methods.
pub trait Lookup<O> {
    fn into_owned(self) -> O;
}

impl<T> Lookup<T> for T {
    fn into_owned(self) -> T {
        self
    }
}

impl<T> HashEqLike<T> for T
where
    T: Hash + Eq,
{
    fn eq(&self, data: &T) -> bool {
        self == data
    }
}

impl<T> HashEqLike<T> for &T
where
    T: Hash + Eq,
{
    fn eq(&self, data: &T) -> bool {
        **self == *data
    }
}

impl<T> HashEqLike<&T> for T
where
    T: Hash + Eq,
{
    fn eq(&self, data: &&T) -> bool {
        *self == **data
    }
}

impl<T> Lookup<T> for &T
where
    T: Clone,
{
    fn into_owned(self) -> T {
        Clone::clone(self)
    }
}

impl<'a, T> HashEqLike<&'a T> for Box<T>
where
    T: ?Sized + Hash + Eq,
    Box<T>: From<&'a T>,
{
    fn eq(&self, data: &&T) -> bool {
        **self == **data
    }
}

impl<'a, T> Lookup<Box<T>> for &'a T
where
    T: ?Sized + Hash + Eq,
    Box<T>: From<&'a T>,
{
    fn into_owned(self) -> Box<T> {
        Box::from(self)
    }
}

impl<'a, T> HashEqLike<&'a T> for Arc<T>
where
    T: ?Sized + Hash + Eq,
    Arc<T>: From<&'a T>,
{
    fn eq(&self, data: &&T) -> bool {
        **self == **data
    }
}

impl<'a, T> Lookup<Arc<T>> for &'a T
where
    T: ?Sized + Hash + Eq,
    Arc<T>: From<&'a T>,
{
    fn into_owned(self) -> Arc<T> {
        Arc::from(self)
    }
}

#[cfg(feature = "triomphe")]
impl<'a, T> HashEqLike<&'a T> for triomphe::Arc<T>
where
    T: ?Sized + Hash + Eq,
    triomphe::Arc<T>: From<&'a T>,
{
    fn eq(&self, data: &&T) -> bool {
        **self == **data
    }
}

#[cfg(feature = "triomphe")]
impl<'a, T> Lookup<triomphe::Arc<T>> for &'a T
where
    T: ?Sized + Hash + Eq,
    triomphe::Arc<T>: From<&'a T>,
{
    fn into_owned(self) -> triomphe::Arc<T> {
        triomphe::Arc::from(self)
    }
}

impl Lookup<String> for &str {
    fn into_owned(self) -> String {
        self.to_owned()
    }
}

#[cfg(feature = "compact_str")]
impl Lookup<compact_str::CompactString> for &str {
    fn into_owned(self) -> compact_str::CompactString {
        compact_str::CompactString::new(self)
    }
}

#[cfg(feature = "compact_str")]
impl HashEqLike<&str> for compact_str::CompactString {
    fn eq(&self, data: &&str) -> bool {
        self == *data
    }
}

#[cfg(feature = "compact_str")]
impl HashEqLike<Cow<'_, str>> for compact_str::CompactString {
    fn eq(&self, data: &Cow<'_, str>) -> bool {
        self.as_str() == data.as_ref()
    }
}

#[cfg(feature = "compact_str")]
impl Lookup<compact_str::CompactString> for Cow<'_, str> {
    fn into_owned(self) -> compact_str::CompactString {
        compact_str::CompactString::new(Cow::into_owned(self))
    }
}

impl HashEqLike<&str> for String {
    fn eq(&self, data: &&str) -> bool {
        self == *data
    }
}

impl<A, T: Hash + Eq + PartialEq<A>> HashEqLike<&[A]> for Vec<T> {
    fn eq(&self, data: &&[A]) -> bool {
        self.len() == data.len() && data.iter().enumerate().all(|(i, a)| &self[i] == a)
    }
}

impl<A: Hash + Eq + PartialEq<T> + Clone + Lookup<T>, T> Lookup<Vec<T>> for &[A] {
    fn into_owned(self) -> Vec<T> {
        self.iter().map(|a| Lookup::into_owned(a.clone())).collect()
    }
}

impl<const N: usize, A, T: Hash + Eq + PartialEq<A>> HashEqLike<[A; N]> for Vec<T> {
    fn eq(&self, data: &[A; N]) -> bool {
        self.len() == data.len() && data.iter().enumerate().all(|(i, a)| &self[i] == a)
    }
}

impl<const N: usize, A: Hash + Eq + PartialEq<T> + Clone + Lookup<T>, T> Lookup<Vec<T>> for [A; N] {
    fn into_owned(self) -> Vec<T> {
        self.into_iter()
            .map(|a| Lookup::into_owned(a.clone()))
            .collect()
    }
}

impl HashEqLike<&Path> for PathBuf {
    fn eq(&self, data: &&Path) -> bool {
        self == data
    }
}

impl Lookup<PathBuf> for &Path {
    fn into_owned(self) -> PathBuf {
        self.to_owned()
    }
}

impl<T: Hash + Eq + Clone> HashEqLike<Cow<'_, T>> for T {
    fn eq(&self, data: &Cow<'_, T>) -> bool {
        self == data.as_ref()
    }
}

impl<T: Clone> Lookup<T> for Cow<'_, T> {
    fn into_owned(self) -> T {
        Cow::into_owned(self)
    }
}

impl HashEqLike<Cow<'_, str>> for String {
    fn eq(&self, data: &Cow<'_, str>) -> bool {
        self.as_str() == data.as_ref()
    }
}

impl Lookup<String> for Cow<'_, str> {
    fn into_owned(self) -> String {
        Cow::into_owned(self)
    }
}

impl HashEqLike<Cow<'_, Path>> for PathBuf {
    fn eq(&self, data: &Cow<'_, Path>) -> bool {
        self.as_path() == data.as_ref()
    }
}

impl Lookup<PathBuf> for Cow<'_, Path> {
    fn into_owned(self) -> PathBuf {
        Cow::into_owned(self)
    }
}

impl<T: Hash + Eq + Clone> HashEqLike<Cow<'_, [T]>> for Box<[T]> {
    fn eq(&self, data: &Cow<'_, [T]>) -> bool {
        self.as_ref() == data.as_ref()
    }
}

impl<T: Clone> Lookup<Box<[T]>> for Cow<'_, [T]> {
    fn into_owned(self) -> Box<[T]> {
        Cow::into_owned(self).into_boxed_slice()
    }
}

impl<T: Hash + Eq + Clone> HashEqLike<Cow<'_, [T]>> for Vec<T> {
    fn eq(&self, data: &Cow<'_, [T]>) -> bool {
        self.as_slice() == data.as_ref()
    }
}

impl<T: Clone> Lookup<Vec<T>> for Cow<'_, [T]> {
    fn into_owned(self) -> Vec<T> {
        Cow::into_owned(self)
    }
}

#[cfg(feature = "persistence")]
mod persistence {
    use std::cell::UnsafeCell;
    use std::fmt;
    use std::hash::BuildHasher;

    use serde::ser::{SerializeMap, SerializeStruct};
    use serde::{Deserialize, de};

    use super::{AtomicDurability, AtomicId, Configuration, IngredientImpl, InternedAt, Value};
    use crate::plumbing::Ingredient;
    use crate::table::memo::MemoTable;
    use crate::zalsa::Zalsa;
    use crate::{Durability, Id, Revision};

    pub struct SerializeIngredient<'db, C>
    where
        C: Configuration,
    {
        pub zalsa: &'db Zalsa,
        pub _ingredient: &'db IngredientImpl<C>,
    }

    impl<C> serde::Serialize for SerializeIngredient<'_, C>
    where
        C: Configuration,
    {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let Self { zalsa, .. } = *self;

            // Counting slots does not access shard metadata or interned fields. The exclusive
            // storage access acquired by `SerializeDatabase::new` keeps the count stable.
            let count = zalsa.table().slots_of::<Value<C>>().count();
            let mut map = serializer.serialize_map(Some(count))?;

            for (_, value) in zalsa.table().slots_of::<Value<C>>() {
                map.serialize_entry(&value.id.load().as_bits(), value)?;
            }

            map.end()
        }
    }

    impl<C> serde::Serialize for Value<C>
    where
        C: Configuration,
    {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let mut value = serializer.serialize_struct("Value,", 3)?;

            let Value {
                fields,
                durability,
                last_interned_at,
                shard: _,
                id: _,
                memos: _,
            } = self;

            // SAFETY: The safety invariant of `Ingredient::serialize` ensures we have exclusive access
            // to the database.
            let fields = unsafe { &*fields.get() };

            let durability = durability.load();
            let last_interned_at = last_interned_at
                .revision()
                .expect("no interned value is claimed while the database is exclusively borrowed");

            value.serialize_field("durability", &durability)?;
            value.serialize_field("last_interned_at", &last_interned_at)?;
            value.serialize_field("fields", &SerializeFields::<C>(fields))?;

            value.end()
        }
    }

    struct SerializeFields<'db, C: Configuration>(&'db C::Fields<'static>);

    impl<C> serde::Serialize for SerializeFields<'_, C>
    where
        C: Configuration,
    {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            C::serialize(IngredientImpl::<C>::from_internal_data(self.0), serializer)
        }
    }

    pub struct DeserializeIngredient<'db, C>
    where
        C: Configuration,
    {
        pub zalsa: &'db mut Zalsa,
        pub ingredient: &'db mut IngredientImpl<C>,
    }

    impl<'de, C> de::DeserializeSeed<'de> for DeserializeIngredient<'_, C>
    where
        C: Configuration,
    {
        type Value = ();

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_map(self)
        }
    }

    impl<'de, C> de::Visitor<'de> for DeserializeIngredient<'_, C>
    where
        C: Configuration,
    {
        type Value = ();

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a map")
        }

        fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            let DeserializeIngredient { zalsa, ingredient } = self;

            while let Some((id, value)) = access.next_entry::<u64, DeserializeValue<C>>()? {
                let id = Id::from_bits(id);
                let (page_idx, _) = crate::table::split_id(id);

                // Determine the value shard.
                let hash = ingredient.hasher.hash_one(&value.fields.0);
                let shard_index = ingredient.shard(hash);

                let mut shard = ingredient.shards.lock(shard_index);

                let value = Value::<C> {
                    shard: shard_index as u16,
                    durability: AtomicDurability::new(value.durability),
                    id: AtomicId::new(id),
                    last_interned_at: InternedAt::new(value.last_interned_at),
                    fields: UnsafeCell::new(value.fields.0),
                    // SAFETY: We only ever access the memos of a value that we allocated through
                    // our `MemoTableTypes`.
                    memos: UnsafeCell::new(unsafe {
                        MemoTable::new(ingredient.memo_table_types())
                    }),
                };

                // Force initialize the relevant page.
                zalsa.table_mut().force_page::<Value<C>>(
                    page_idx,
                    ingredient.ingredient_index(),
                    ingredient.memo_table_types(),
                );

                // Initialize the slot.
                //
                // SAFETY: We have a mutable reference to the database.
                let (allocated_id, value) = unsafe {
                    zalsa
                        .table()
                        .page(page_idx)
                        .allocate(page_idx, |_| value)
                        .unwrap_or_else(|_| panic!("serialized an invalid `Id`: {id:?}"))
                };

                assert_eq!(
                    allocated_id.index(),
                    id.index(),
                    "values are serialized in allocation order"
                );

                // Insert the newly allocated value into our ingredient.
                //
                // SAFETY: We hold this ingredient's shard lock, and `value` is the live, stable
                // value that we just allocated in that shard, its fields hashing to `hash`.
                unsafe { IngredientImpl::<C>::insert_value(&mut shard, hash, value) };
            }

            Ok(())
        }
    }

    #[derive(Deserialize)]
    #[serde(rename = "Value")]
    pub struct DeserializeValue<C: Configuration> {
        durability: Durability,
        last_interned_at: Revision,
        #[serde(bound = "C: Configuration")]
        fields: DeserializeFields<C>,
    }

    struct DeserializeFields<C: Configuration>(C::Fields<'static>);

    impl<'de, C> serde::Deserialize<'de> for DeserializeFields<C>
    where
        C: Configuration,
    {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            C::deserialize(deserializer)
                .map(DeserializeFields)
                .map_err(de::Error::custom)
        }
    }
}

#[cfg(all(not(feature = "shuttle"), target_pointer_width = "64"))]
mod _static_assertions {
    use std::mem;

    use super::{Configuration, Member, Value};
    use crate::{Id, plumbing};

    // A value is its fields, the ID, `last_interned_at`, and one word holding the shard, the
    // durability and the 4-byte memo row: 24 bytes besides the fields.
    const _: [(); mem::size_of::<Value<DummyConfiguration>>()] = [(); 3 * mem::size_of::<usize>()];

    // A reusable value's entry in its shard's sweep list.
    const _: [(); mem::size_of::<Member>()] = [(); 16];

    struct DummyConfiguration;

    // SAFETY: The fields are `[u8; 1]` for every database lifetime.
    unsafe impl Configuration for DummyConfiguration {
        const LOCATION: crate::ingredient::Location =
            crate::ingredient::Location { file: "", line: 0 };
        const DEBUG_NAME: &'static str = "";
        const PERSIST: bool = false;

        type Fields<'db> = [u8; 1];
        type Struct<'db> = Id;

        fn serialize<S>(_: &Self::Fields<'_>, _: S) -> Result<S::Ok, S::Error>
        where
            S: plumbing::serde::Serializer,
        {
            unreachable!("`PERSIST` is false, so fields are never serialized")
        }

        fn deserialize<'de, D>(_: D) -> Result<Self::Fields<'static>, D::Error>
        where
            D: plumbing::serde::Deserializer<'de>,
        {
            unreachable!("`PERSIST` is false, so fields are never deserialized")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestConfiguration;

    // SAFETY: The fields are `()` for every database lifetime.
    unsafe impl Configuration for TestConfiguration {
        const LOCATION: crate::ingredient::Location = crate::ingredient::Location {
            file: file!(),
            line: line!(),
        };
        const DEBUG_NAME: &'static str = "TestConfiguration";
        const PERSIST: bool = false;
        const REVISIONS: NonZeroUsize = NonZeroUsize::new(2).unwrap();

        type Fields<'db> = ();
        type Struct<'db> = Id;

        fn serialize<S>(_value: &Self::Fields<'_>, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: plumbing::serde::Serializer,
        {
            unreachable!("`PERSIST` is false, so fields are never serialized")
        }

        fn deserialize<'de, D>(_deserializer: D) -> Result<Self::Fields<'static>, D::Error>
        where
            D: plumbing::serde::Deserializer<'de>,
        {
            unreachable!("`PERSIST` is false, so fields are never deserialized")
        }
    }

    #[test]
    fn revision_queue_records_each_revision_once() {
        let queue = RevisionQueue::new(TestConfiguration::REVISIONS);
        let revision = Revision::start().next();

        // Simulate two threads that both passed the fast-path check before taking the lock.
        queue.record_cold(revision);
        queue.record_cold(revision);

        assert_eq!(queue.revisions[0].load(), revision);
        assert_eq!(queue.revisions[1].load(), Revision::start());
    }

    fn revision(n: usize) -> Revision {
        Revision::from(n)
    }

    #[cfg(not(feature = "shuttle"))]
    #[test]
    fn interned_at_keeps_revisions_and_forever() {
        for at in [Revision::start(), revision(2), revision(1 << 30), Revision::max()] {
            let word = InternedAt::new(at);
            assert_eq!(word.revision(), Some(at));
            assert!(word.is_fresh(at));
            let later = if at == Revision::max() { at } else { at.next() };
            assert_eq!(word.is_fresh(later), at == Revision::max());
        }
    }

    /// A pin and a claim exclude each other; a matching reader's unpin records the current
    /// revision, so the stale claim fails after it; a non-matching reader's leaves the value as
    /// it was, so the claim succeeds.
    #[cfg(not(feature = "shuttle"))]
    #[test]
    fn pins_and_claims_exclude_each_other() {
        let stale = revision(2);
        let current = revision(5);

        let word = InternedAt::new(stale);
        assert!(matches!(word.pin(current, || true), Seen::Pinned));
        assert!(!word.claim(stale), "a pinned value can't be claimed");
        assert!(matches!(word.pin(current, || true), Seen::Pinned));
        assert!(!word.unpin(None));
        assert!(!word.claim(stale), "still pinned once");
        assert!(!word.unpin(None));
        assert_eq!(word.revision(), Some(stale), "a non-matching read changes nothing");
        assert!(word.claim(stale));
        assert_eq!(word.revision(), None);
        assert!(matches!(word.pin(current, || true), Seen::Skip), "claimed");
        assert_eq!(word.raise(current), None);
        word.publish(current);
        assert!(matches!(word.pin(current, || true), Seen::Fresh));

        let word = InternedAt::new(stale);
        assert!(matches!(word.pin(current, || false), Seen::Skip), "another key's");
        assert!(matches!(word.pin(current, || true), Seen::Pinned));
        assert!(matches!(word.pin(current, || true), Seen::Pinned));
        assert!(word.unpin(Some(current)), "the first matching reader records it");
        assert!(matches!(word.pin(current, || true), Seen::Fresh));
        assert!(!word.unpin(Some(current)), "the second finds it recorded");
        assert_eq!(word.revision(), Some(current));
        assert!(!word.claim(stale));
        assert_eq!(word.0.load(Ordering::Relaxed) & PINS, 0, "every pin released");

        let word = InternedAt::new(stale);
        assert_eq!(word.raise(current), Some(true));
        assert_eq!(word.raise(current), Some(false));
        assert_eq!(word.raise(stale), Some(false), "never lowered");
        assert_eq!(word.revision(), Some(current));
    }

    #[cfg(not(feature = "shuttle"))]
    #[test]
    fn saturated_pins_skip() {
        let stale = revision(2);
        let current = revision(3);
        let word = InternedAt::new(stale);
        for _ in 0..PINS {
            assert!(matches!(word.pin(current, || true), Seen::Pinned));
        }
        assert!(matches!(word.pin(current, || true), Seen::Skip));
        for _ in 0..PINS {
            word.unpin(None);
        }
        assert!(word.claim(stale));
    }

    /// A pin dropped by unwinding (a panicking `Eq`) is released.
    #[cfg(not(feature = "shuttle"))]
    #[test]
    fn pin_released_on_unwind() {
        let stale = revision(2);
        let word = InternedAt::new(stale);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(matches!(word.pin(revision(3), || true), Seen::Pinned));
            let _pin = Pin(&word);
            panic!("user `Eq` panicked");
        }));
        assert!(result.is_err());
        assert!(word.claim(stale));
    }

    /// Threads pinning and recording one stale value while one claims it: either the claim wins
    /// and no reader read under it, or a reader recorded the value and the claim failed.
    #[cfg(not(feature = "shuttle"))]
    #[test]
    fn racing_pins_and_claim() {
        let stale = revision(2);
        let current = revision(4);
        for round in 0..200 {
            let word = InternedAt::new(stale);
            let reading = std::sync::atomic::AtomicUsize::new(0);
            let read_while_claimed = std::sync::atomic::AtomicBool::new(false);
            let claimed = std::sync::atomic::AtomicBool::new(false);
            std::thread::scope(|scope| {
                for reader in 0..4 {
                    let (word, reading, read_while_claimed, claimed) =
                        (&word, &reading, &read_while_claimed, &claimed);
                    scope.spawn(move || {
                        for _ in 0..100 {
                            if let Seen::Pinned = word.pin(current, || true) {
                                reading.fetch_add(1, Ordering::SeqCst);
                                if claimed.load(Ordering::SeqCst) {
                                    read_while_claimed.store(true, Ordering::SeqCst);
                                }
                                reading.fetch_sub(1, Ordering::SeqCst);
                                word.unpin((reader == 0 && round % 2 == 0).then_some(current));
                            }
                        }
                    });
                }
                scope.spawn(|| {
                    for _ in 0..100 {
                        if word.claim(stale) {
                            claimed.store(true, Ordering::SeqCst);
                            assert_eq!(reading.load(Ordering::SeqCst), 0, "round {round}");
                            break;
                        }
                    }
                });
            });
            assert!(!read_while_claimed.load(Ordering::SeqCst), "round {round}");
            if claimed.load(Ordering::SeqCst) {
                assert_eq!(word.revision(), None);
            } else {
                assert_eq!(word.0.load(Ordering::Relaxed) & PINS, 0);
            }
        }
    }
}
