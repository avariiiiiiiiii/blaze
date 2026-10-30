//! The table under [`crate::Symbol`] and [`crate::Mapped`]: interned values as byte strings
//! with `u32` IDs that mean the same in every process that opens one image
//! (docs/v2/plan/design/restore.md, "IDs are stable across restarts").
//!
//! An ID is one of three kinds:
//!
//! - `1..=statics`: compiled in (the declared symbols). The section's schema names the list,
//!   so an image is only restored by a blaze with the same one.
//! - `first..base.end` with its live bit set: restored from the image, read in place.
//! - any other below `next`: made by this process, in the overlay: an append-only arena behind
//!   [`Store`]'s lock-free index, and a column from ID to record.
//!
//! A lookup probes the mapped index (SwissTable groups of eight tag bytes beside eight IDs,
//! read only), then the overlay's. A new value takes an ID the image's GC freed first, then the
//! next: no ID of a live value is ever given to another, and nothing is renumbered.
//!
//! GC is [`Table::write`]: the image keeps the values whose IDs the owners of the other
//! sections marked [`Live`], each at its own ID; every other ID becomes free, for the values
//! the next process makes. The process that writes keeps everything it has: its handles stay
//! good until it exits.
//!
//! Cost: a value new in this process takes a 16-byte header beside its bytes and an 8-byte
//! column slot; a value in the image takes 12 bytes (its start and order key), one live bit
//! and 5 to 10 bytes of index. A lookup is one hash of the bytes, one probe of the mapped index
//! (a control word, then the bytes compared) and, for a value not in the image, one of the
//! overlay's. Reading a value's bytes is a range check, one live bit and two starts.

use std::{alloc::Layout, fmt, ptr::NonNull};

use blaze_format::SchemaHash;
use blaze_image::{Corrupt, Image, Refused, Section, SectionId, SectionWriter, Stable};

use crate::{
    arena::{Entry, Store},
    swiss::{self, Ctrl},
    sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering},
};

/// What every table of one kind shares.
pub(crate) struct Shape {
    /// IDs `1..=statics` are compiled in.
    pub(crate) statics: u32,
    /// The bytes of compiled-in ID `id`.
    pub(crate) static_bytes: fn(u32) -> &'static [u8],
    /// The order key of a value's bytes.
    pub(crate) key: fn(&[u8]) -> u64,
    /// The alignment of a value's bytes, at most [`MAX_ALIGN`].
    pub(crate) align: usize,
    /// Every value's length is a multiple of this, itself a nonzero multiple of `align`.
    pub(crate) size: usize,
    /// Whether every value is UTF-8, checked when an image is restored.
    pub(crate) text: bool,
}

pub(crate) const MAX_ALIGN: usize = 16;

/// Slots in a group of the mapped index: tag bytes 0 to 7 of its control word.
const GROUP: usize = 8;

/// The hash of a value's bytes, in every table, mapped or not. Defined here, not by a hasher
/// crate, so an image's index means the same to every blaze that opens it: FxHash's
/// multiply-rotate over 8-byte words, the length first, then murmur3's 64-bit finalizer so the
/// tag (the top 7 bits) and the home group (the low ones) are both well mixed.
pub(crate) fn value_hash(bytes: &[u8]) -> u64 {
    const K: u64 = 0xf135_7aea_2e62_a9c5;
    let step = |h: u64, word: u64| (h ^ word).wrapping_mul(K).rotate_left(26);
    let mut h = (bytes.len() as u64).wrapping_mul(K);
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        h = step(h, u64::from_le_bytes(word.try_into().expect("8 bytes")));
    }
    let rest = words.remainder();
    if !rest.is_empty() {
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        h = step(h, u64::from_le_bytes(last));
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

blaze_format::record! {
    /// What the image holds of one table, at its section's start: where each column is in the
    /// section, and the counts.
    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    struct Root: 1 {
        /// `count + 1` `u32`s: value `i`'s bytes are `starts[i]..starts[i + 1]` of `bytes`.
        starts: u64,
        /// `count` `u64` order keys.
        keys: u64,
        /// `ceil(count / 64)` `u64`s: bit `i` is set when ID `first + i` holds a value.
        live: u64,
        /// `groups` control words of the index.
        ctrl: u64,
        /// `groups * 8` IDs of the index.
        ids: u64,
        /// `frees` free IDs, ascending.
        free: u64,
        bytes: u64,
        bytes_len: u64,
        first: u32,
        count: u32,
        groups: u32,
        frees: u32,
        statics: u32,
        align: u32,
    }
}

/// What a table's section means beyond [`Root`]: the columns, the index and [`value_hash`].
/// A change to any of them changes this text, and so every table's schema.
pub(crate) const LAYOUT: &str = "intern table 1: Root at 0; starts u32, keys u64, live u64 bits, \
     ctrl u64 groups of 8 tags (0x80 | hash >> 57) probed linearly from hash & mask, ids u32, \
     free u32 ascending, bytes; value_hash multiply-rotate with murmur3 finalizer";

/// [`Root`]'s record schema.
pub(crate) const ROOT: SchemaHash = <Root as blaze_format::Record>::SCHEMA;

/// The table's part in the image this process restored, read in place; empty for a fresh one.
#[derive(Default)]
struct Base {
    /// One past the last ID the image holds.
    end: u32,
    starts: &'static [u32],
    keys: &'static [u64],
    live: &'static [u64],
    ctrl: &'static [u64],
    ids: &'static [u32],
    free: &'static [u32],
    bytes: &'static [u8],
}

impl Base {
    fn is_live(&self, index: usize) -> bool {
        self.live[index / 64] >> (index % 64) & 1 == 1
    }
}

/// A value this process made: its header, then its bytes.
#[repr(C, align(16))]
struct Head {
    key: u64,
    id: u32,
    len: u32,
}

const _: () = assert!(size_of::<Head>() == MAX_ALIGN);

impl Head {
    /// The header at `at`, a value of a table's store.
    fn at(at: NonNull<u8>) -> &'static Head {
        // SAFETY: every value of a table's `Store` is a `Head` followed by its bytes, written
        // before it was published (`Table::insert`) and never moved or freed.
        unsafe { at.cast::<Head>().as_ref() }
    }

    fn bytes(&'static self) -> &'static [u8] {
        let after = std::ptr::from_ref(self).wrapping_add(1).cast::<u8>();
        // SAFETY: `Table::insert` allocates `len` bytes right after the header, in the same
        // allocation, and writes them before publishing; the arena never frees them.
        unsafe { std::slice::from_raw_parts(after, self.len as usize) }
    }
}

fn record_layout(len: usize) -> Layout {
    Layout::from_size_align(size_of::<Head>() + len, MAX_ALIGN).expect("an interned value's layout")
}

/// ID to header for the values this process made, in buckets that double: bucket `b` holds
/// `FIRST_BUCKET << b` slots, allocated the first time an ID in it is made.
struct Column {
    buckets: [AtomicPtr<AtomicPtr<Head>>; BUCKETS],
}

const FIRST_BUCKET: usize = 1024;
/// Enough for every `u32`.
const BUCKETS: usize = 23;

/// The bucket and slot of index `index`.
fn locate(index: usize) -> (usize, usize) {
    let k = index / FIRST_BUCKET + 1;
    let bucket = (usize::BITS - 1 - k.leading_zeros()) as usize;
    (bucket, index - FIRST_BUCKET * ((1 << bucket) - 1))
}

#[cfg(not(blaze_check))]
fn slots(len: usize) -> Box<[AtomicPtr<Head>]> {
    // SAFETY: all-zero bytes are a null `AtomicPtr`.
    unsafe { Box::<[AtomicPtr<Head>]>::new_zeroed_slice(len).assume_init() }
}

/// The checker's atomics are not plain words, so a model build makes each one.
#[cfg(blaze_check)]
fn slots(len: usize) -> Box<[AtomicPtr<Head>]> {
    // alloc: the model checker's build only, once a table
    (0..len).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect()
}

impl Column {
    fn new() -> Column {
        Column { buckets: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())) }
    }

    fn get(&self, index: usize) -> Option<&'static Head> {
        let (bucket, slot) = locate(index);
        let bucket = self.buckets.get(bucket)?.load(Ordering::Acquire);
        if bucket.is_null() {
            return None;
        }
        // SAFETY: a published bucket is leaked, `FIRST_BUCKET << b` slots long, and `slot` is
        // below that (`locate`).
        let slot = unsafe { &*bucket.add(slot) };
        let at = NonNull::new(slot.load(Ordering::Acquire))?;
        Some(Head::at(at.cast()))
    }

    fn set(&self, index: usize, head: NonNull<Head>) {
        let (bucket, slot) = locate(index);
        let bucket = self.bucket(bucket);
        // SAFETY: as in `get`: `bucket` is a published bucket and `slot` is in it.
        let slot = unsafe { &*bucket.add(slot) };
        // Release: a reader that loads the header sees it written.
        slot.store(head.as_ptr(), Ordering::Release);
    }

    fn bucket(&self, bucket: usize) -> *mut AtomicPtr<Head> {
        let cell = &self.buckets[bucket];
        let found = cell.load(Ordering::Acquire);
        if !found.is_null() {
            return found;
        }
        let len = FIRST_BUCKET << bucket;
        let made = Box::into_raw(slots(len)).cast::<AtomicPtr<Head>>();
        match cell.compare_exchange(std::ptr::null_mut(), made, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => made,
            Err(other) => {
                let made = std::ptr::slice_from_raw_parts_mut(made, len);
                // SAFETY: `made` is the `Box` of `len` slots made just above, never published.
                drop(unsafe { Box::from_raw(made) });
                other
            }
        }
    }
}

/// The IDs the owners of an image's other sections still name, marked before the image is
/// written: a value whose ID is not marked is not kept.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Live {
    bits: Vec<u64>,
}

impl Live {
    pub fn new() -> Live {
        Live::default()
    }

    pub fn mark_id(&mut self, id: u32) {
        let word = id as usize / 64;
        if word >= self.bits.len() {
            self.bits.resize(word + 1, 0);
        }
        self.bits[word] |= 1 << (id % 64);
    }

    pub fn contains(&self, id: u32) -> bool {
        self.bits.get(id as usize / 64).is_some_and(|word| word >> (id % 64) & 1 == 1)
    }

    /// How many IDs are marked.
    pub fn count(&self) -> usize {
        self.bits.iter().map(|word| word.count_ones() as usize).sum()
    }
}

/// Why a table was not restored from an image. The table starts empty, and so must every
/// section whose values name its IDs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RestoreError {
    Refused(Refused),
    /// A group of the table's section failed its CRC: the whole table is dropped.
    Corrupt(Corrupt),
    /// The section's bytes are not a table of this kind.
    Invalid { section: SectionId, what: &'static str },
    /// Values were interned in this process before the restore, under IDs the image may hold.
    Started { section: SectionId },
}

impl From<Corrupt> for RestoreError {
    fn from(corrupt: Corrupt) -> RestoreError {
        RestoreError::Corrupt(corrupt)
    }
}

impl From<Refused> for RestoreError {
    fn from(refused: Refused) -> RestoreError {
        RestoreError::Refused(refused)
    }
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreError::Refused(refused) => refused.fmt(f),
            RestoreError::Corrupt(corrupt) => corrupt.fmt(f),
            RestoreError::Invalid { section, what } => write!(f, "{section} is not a table: {what}"),
            RestoreError::Started { section } => {
                write!(f, "{section} was restored after values were interned: restore it first")
            }
        }
    }
}

impl std::error::Error for RestoreError {}

pub use blaze_image::{Jobs, serial};

/// Groups one job of a restore checks: 1 MiB at 64 KiB groups, about 37 µs of CRC32C, so
/// handing a job out is a few percent of it.
const CHUNK_GROUPS: usize = 16;
/// Entries of a column one job of a restore validates: about 16k values at about 25
/// instructions each, near 40 µs, so a million symbols are 64 jobs, enough to fill a socket.
const CHUNK: usize = 1 << 14;

/// `check` over `0..total` in chunks of `chunk`, run by `jobs`. The error is the first chunk's
/// that failed, so the same bytes give the same error however many workers ran.
fn in_chunks<E: Send>(
    jobs: Jobs<'_>,
    total: usize,
    chunk: usize,
    check: impl Fn(std::ops::Range<usize>) -> Result<(), E> + Sync,
) -> Result<(), E> {
    let n = total.div_ceil(chunk);
    if n <= 1 {
        return check(0..total);
    }
    let failed = std::sync::Mutex::new(None::<(usize, E)>);
    jobs(n, &|job| {
        if let Err(error) = check(job * chunk..((job + 1) * chunk).min(total)) {
            let mut failed = failed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if failed.as_ref().is_none_or(|(at, _)| job < *at) {
                *failed = Some((job, error));
            }
        }
    });
    match failed.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner) {
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}

pub(crate) struct Table {
    shape: Shape,
    /// The first ID not compiled in.
    first: u32,
    base: Base,
    /// The next ID no value has had.
    next: AtomicU32,
    /// How many of `base.free` were taken.
    reused: AtomicUsize,
    store: Store,
    fresh: Column,
}

impl Table {
    /// An empty table, its compiled-in values in the overlay's index.
    pub(crate) fn new(shape: Shape) -> Table {
        let first = shape.statics + 1;
        let table = Table {
            first,
            base: Base { end: first, ..Base::default() },
            next: AtomicU32::new(first),
            reused: AtomicUsize::new(0),
            store: Store::new(),
            fresh: Column::new(),
            shape,
        };
        for id in 1..first {
            let bytes = (table.shape.static_bytes)(id);
            let same = |at: NonNull<u8>| Head::at(at).bytes() == bytes;
            // A value compiled in twice is the first.
            if let Entry::Vacant(vacant) = table.store.entry(value_hash(bytes), same) {
                table.insert(vacant, id, bytes);
            }
        }
        table
    }

    /// The table `section` of `image` holds, read in place: its section checked whole, then
    /// its columns checked against each other (every start in bounds and in order, every live
    /// value UTF-8 for text, every index slot a live ID, every free ID not live).
    pub(crate) fn restore<S: Stable>(
        shape: Shape,
        image: &'static Image<S>,
        section: &Section,
        jobs: Jobs<'_>,
    ) -> Result<Table, RestoreError> {
        let groups = section.groups as usize;
        in_chunks(jobs, groups, CHUNK_GROUPS, |at| image.check(section, at.start as u32..at.end as u32))?;
        let root = *image.value::<Root>(section, 0)?;
        let invalid = |what| RestoreError::Invalid { section: section.id, what };
        if root.statics != shape.statics || root.first != shape.statics + 1 || root.align as usize != shape.align {
            return Err(invalid("a table of another shape"));
        }
        let end = root.first.checked_add(root.count).filter(|&end| end < u32::MAX);
        let end = end.ok_or(invalid("more IDs than a u32 holds"))?;
        let count = u64::from(root.count);
        let groups = u64::from(root.groups);
        if !groups.is_power_of_two() {
            return Err(invalid("an index that is not a power of two of groups"));
        }
        let base = Base {
            end,
            starts: image.slice::<u32>(section, root.starts, count + 1)?,
            keys: image.slice::<u64>(section, root.keys, count)?,
            live: image.slice::<u64>(section, root.live, count.div_ceil(64))?,
            ctrl: image.slice::<u64>(section, root.ctrl, groups)?,
            ids: image.slice::<u32>(section, root.ids, groups * GROUP as u64)?,
            free: image.slice::<u32>(section, root.free, u64::from(root.frees))?,
            bytes: image.range(section, root.bytes, root.bytes_len)?,
        };
        let table = Table {
            first: root.first,
            base,
            next: AtomicU32::new(end),
            reused: AtomicUsize::new(0),
            store: Store::new(),
            fresh: Column::new(),
            shape,
        };
        table.validate(jobs).map_err(invalid)?;
        Ok(table)
    }

    /// Makes `cell`'s table the one `image`'s section `id` holds. Refused once `cell` has one:
    /// its IDs may be the image's.
    pub(crate) fn restore_into<S: Stable>(
        cell: &std::sync::OnceLock<Table>,
        shape: Shape,
        image: &'static Image<S>,
        id: SectionId,
        schema: SchemaHash,
        jobs: Jobs<'_>,
    ) -> Result<(), RestoreError> {
        let started = RestoreError::Started { section: id };
        if cell.get().is_some() {
            return Err(started);
        }
        let section = image.section(id, schema)?;
        let table = Table::restore(shape, image, &section, jobs)?;
        cell.set(table).map_err(|_| started)
    }

    /// The checks that make every read of the restored columns in bounds, in O(IDs + slots)
    /// with no value hashed, each column split into chunks run by `jobs`. That the index
    /// reaches every live value under its own hash is the section's CRC's to guard: a slot that
    /// does not costs a second ID for that value, never a wrong value.
    fn validate(&self, jobs: Jobs<'_>) -> Result<(), &'static str> {
        let base = &self.base;
        let count = (base.end - self.first) as usize;
        let align = self.shape.align;
        if !(base.bytes.as_ptr() as usize).is_multiple_of(align) {
            return Err("values misaligned");
        }
        if base.starts[0] != 0 || base.starts[count] as usize != base.bytes.len() {
            return Err("starts that do not span the values");
        }
        if count % 64 != 0 && base.live[count / 64] >> (count % 64) != 0 {
            return Err("live bits past the last ID");
        }
        in_chunks(jobs, count, CHUNK, |indices| {
            for index in indices {
                let (start, end) = (base.starts[index] as usize, base.starts[index + 1] as usize);
                if end < start || !start.is_multiple_of(align) || !(end - start).is_multiple_of(self.shape.size) {
                    return Err("a value's bytes out of order or misaligned");
                }
                match base.is_live(index) {
                    true if self.shape.text && std::str::from_utf8(&base.bytes[start..end]).is_err() => {
                        return Err("a value that is not UTF-8");
                    }
                    true => {}
                    false if end != start => return Err("bytes for a free ID"),
                    false => {}
                }
            }
            Ok(())
        })?;
        in_chunks(jobs, base.free.len(), CHUNK, |at| {
            for at in at {
                let id = base.free[at];
                let in_range = id >= self.first && id < base.end;
                let ascending = at == 0 || base.free[at - 1] < id;
                if !in_range || !ascending || base.is_live((id - self.first) as usize) {
                    return Err("a free ID that is live, repeated or out of range");
                }
            }
            Ok(())
        })?;
        let full = std::sync::atomic::AtomicUsize::new(0);
        in_chunks(jobs, base.ctrl.len(), CHUNK / GROUP, |groups| {
            let mut here = 0;
            for group in groups {
                let word = base.ctrl[group];
                for byte in 0..GROUP {
                    let tag = (word >> (byte * 8)) & 0xff;
                    if tag == 0 {
                        continue;
                    }
                    let id = base.ids[group * GROUP + byte];
                    let known = (1..self.first).contains(&id)
                        || (id >= self.first && id < base.end && base.is_live((id - self.first) as usize));
                    if tag & 0x80 == 0 || !known {
                        return Err("an index slot that names no value");
                    }
                    here += 1;
                }
            }
            full.fetch_add(here, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        })?;
        let full = full.into_inner();
        if full * 2 > base.ids.len() || (self.first > 1 && full == 0) {
            return Err("an index more than half full, or without the compiled-in values");
        }
        Ok(())
    }

    /// The ID of `bytes`, made if it has none.
    pub(crate) fn intern(&self, bytes: &[u8]) -> u32 {
        let hash = value_hash(bytes);
        if let Some(id) = self.base_find(hash, bytes) {
            return id;
        }
        let same = |at: NonNull<u8>| Head::at(at).bytes() == bytes;
        match self.store.entry(hash, same) {
            Entry::Found(at) => Head::at(at).id,
            Entry::Vacant(vacant) => {
                let id = self.take_id();
                self.insert(vacant, id, bytes);
                id
            }
        }
    }

    /// The ID of `bytes`, if it has one.
    pub(crate) fn find(&self, bytes: &[u8]) -> Option<u32> {
        let hash = value_hash(bytes);
        self.base_find(hash, bytes).or_else(|| {
            self.store.find(hash, |at| Head::at(at).bytes() == bytes).map(|at| Head::at(at).id)
        })
    }

    /// Writes `bytes` as value `id` into the overlay and publishes it. The column's slot is set
    /// before the index publishes the value, so a reader that finds the ID finds its bytes.
    fn insert(&self, vacant: crate::arena::Vacant<'_>, id: u32, bytes: &[u8]) {
        let key = (self.shape.key)(bytes);
        let len = u32::try_from(bytes.len()).expect("an interned value of 4 GiB or more");
        let first = self.first;
        vacant.insert(record_layout(bytes.len()), |to| {
            let head = to.cast::<Head>();
            // SAFETY: `to` is fresh memory of `record_layout(len)`: aligned for a `Head`, with
            // room for it and `len` bytes after it.
            unsafe { head.write(Head { key, id, len }) };
            let after = head.as_ptr().wrapping_add(1).cast::<u8>();
            // SAFETY: the `len` bytes after the header are in the allocation and fresh, so
            // they cannot overlap `bytes`.
            unsafe { after.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len()) };
            if id >= first {
                self.fresh.set((id - first) as usize, head);
            }
        });
    }

    fn take_id(&self) -> u32 {
        let free = self.base.free;
        if self.reused.load(Ordering::Relaxed) < free.len() {
            let at = self.reused.fetch_add(1, Ordering::Relaxed);
            if let Some(&id) = free.get(at) {
                return id;
            }
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(id < u32::MAX, "more than 2^32 - 2 values interned in one table");
        id
    }

    fn base_find(&self, hash: u64, bytes: &[u8]) -> Option<u32> {
        let ctrl = self.base.ctrl;
        if ctrl.is_empty() {
            return None;
        }
        swiss::find(hash, swiss::tag(hash), ctrl.len(), |at| Ctrl::<GROUP>(ctrl[at]), |at| {
            let id = self.base.ids[at.index()];
            (self.get(id) == Some(bytes)).then_some(id)
        })
    }

    /// The bytes of `id`, `None` when it names no value: never made, or freed by the GC of the
    /// image this process restored and not made again.
    #[inline]
    pub(crate) fn get(&self, id: u32) -> Option<&'static [u8]> {
        if id == 0 {
            return None;
        }
        if id < self.first {
            return Some((self.shape.static_bytes)(id));
        }
        let index = (id - self.first) as usize;
        let base = &self.base;
        if id < base.end && base.is_live(index) {
            return Some(&base.bytes[base.starts[index] as usize..base.starts[index + 1] as usize]);
        }
        self.fresh.get(index).map(Head::bytes)
    }

    /// [`Table::get`] of an ID a handle holds, which must name a value.
    #[inline]
    pub(crate) fn bytes(&self, id: u32) -> &'static [u8] {
        self.get(id).unwrap_or_else(|| self.lost(id))
    }

    #[cold]
    #[inline(never)]
    fn lost(&self, id: u32) -> ! {
        panic!(
            "interned ID {id} names no value: the image's GC freed it while a value still named \
             it, or it was never made (IDs below {} were)",
            self.next.load(Ordering::Relaxed)
        )
    }

    /// The order key of `id`'s value.
    pub(crate) fn key(&self, id: u32) -> u64 {
        if id < self.first {
            return (self.shape.key)(self.bytes(id));
        }
        let index = (id - self.first) as usize;
        if id < self.base.end && self.base.is_live(index) {
            return self.base.keys[index];
        }
        self.fresh.get(index).map_or_else(|| self.lost(id), |head| head.key)
    }

    /// One past the highest ID made so far.
    pub(crate) fn end(&self) -> u32 {
        self.next.load(Ordering::Acquire)
    }

    /// Writes the table into `out`, an empty section: every value whose ID `live` marks, at its
    /// ID, and every other ID free. Values interned while this runs, after it read `next`, are
    /// not in it: nothing written before named them.
    pub(crate) fn write(&self, live: &Live, out: &mut SectionWriter<'_>) {
        let end = self.end();
        let count = (end - self.first) as usize;
        let mut starts = Vec::with_capacity(count + 1);
        let mut keys = Vec::with_capacity(count);
        // alloc: `Table::write`'s columns, once an image written at the daemon's stop
        let mut live_bits = vec![0u64; count.div_ceil(64)];
        // alloc: as the starts
        let mut bytes = Vec::new();
        // alloc: as the starts
        let mut free = Vec::new();
        let mut kept = Vec::with_capacity(live.count().min(count));
        starts.push(0u32);
        for index in 0..count {
            let id = self.first + index as u32;
            if live.contains(id) {
                bytes.extend_from_slice(self.bytes(id));
                keys.push(self.key(id));
                live_bits[index / 64] |= 1 << (index % 64);
                kept.push(id);
            } else {
                keys.push(0);
                free.push(id);
            }
            starts.push(u32::try_from(bytes.len()).expect("a table of 4 GiB of values or more"));
        }

        let slots = (self.first as usize - 1 + kept.len()) * 2;
        let groups = slots.div_ceil(GROUP).next_power_of_two().max(1);
        // alloc: `Table::write`'s index, once an image written
        let mut ctrl = vec![0u64; groups];
        // alloc: as the index
        let mut ids = vec![0u32; groups * GROUP];
        let mut put = |id: u32, value: &[u8], dedupe: bool| {
            let hash = value_hash(value);
            let tag = swiss::tag(hash);
            let word = |at: usize| Ctrl::<GROUP>(ctrl[at]);
            if dedupe && swiss::find(hash, tag, groups, word, |at| (self.get(ids[at.index()]) == Some(value)).then_some(())).is_some() {
                return;
            }
            let at = swiss::vacant(hash, groups, |at| Ctrl::<GROUP>(ctrl[at]));
            ctrl[at.group] = at.tagged(tag).0;
            ids[at.index()] = id;
        };
        // A value compiled in twice is the first, as in the overlay.
        for id in 1..self.first {
            put(id, (self.shape.static_bytes)(id), true);
        }
        for &id in &kept {
            put(id, self.bytes(id), false);
        }

        let root = out.value(&Root::default());
        assert_eq!(root, 0, "a table is written into an empty section");
        let at = Root {
            starts: out.slice(&starts),
            keys: out.slice(&keys),
            live: out.slice(&live_bits),
            ctrl: out.slice(&ctrl),
            ids: out.slice(&ids),
            free: out.slice(&free),
            bytes: out.align(MAX_ALIGN.max(self.shape.align)),
            bytes_len: bytes.len() as u64,
            first: self.first,
            count: count as u32,
            groups: groups as u32,
            frees: free.len() as u32,
            statics: self.first - 1,
            align: self.shape.align as u32,
        };
        out.bytes(&bytes);
        out.patch_value(root, &at);
    }
}

#[cfg(test)]
mod tests;
