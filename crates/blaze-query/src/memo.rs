//! The memo store (docs/v2/plan/design/data.md, "query database"): every memo a `u32` row of
//! a hot column and a cold one, its value a row of a value column, and the [`Slot`]s that list
//! a key's memos.
//!
//! - A memo's hot row is what verification and a hit read: the fingerprint, where the value is,
//!   and the memo's edges as one range of a per-worker edge segment (or of the image). Its cold
//!   row is what only writers read: the key and the segment its edges hold.
//! - A value's location is one word in the hot row: the value's row and that row's generation.
//!   Evicting, refilling and moving a value to disk are each a compare-and-swap of the word, so
//!   a writer that saw a value swaps it only if the word still names that very value, however
//!   often its row was reused since.
//! - Edges and values are copied once into this thread's segments ([`crate::segment`]); a
//!   memo or value let go of lets go of its hold, and a segment is freed whole at collection.
//! - Memos are freed only by [`Database::collect`](crate::Database::collect): unlinked from
//!   their slots, then (once a grace has passed) checked against the open views, then freed
//!   once a grace begun after that check has passed. A view's results name memos by ID, so a
//!   memo a view took from a slot just before it was unlinked stays while the view is open.

use std::{
    ptr::{self, NonNull},
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_format::SchemaHash;
use blaze_ids::{ContentId, Fingerprint};
use blaze_reclaim::{Grace, Guard};

use crate::{
    QueryKey, Stored,
    segment::{self, Segment, Segments, Span, lane},
    table::{EDGE_RECORD, EdgeRecord, Free, FxMap, Generations, Ids, Rows, ShortLock},
};

/// A memo: its row in the store's hot and cold columns.
pub(crate) type MemoId = u32;

/// A value's location: `generation << 32 | row + 1`, and 0 for none (evicted).
fn word(generation: u32, row: u32) -> u64 {
    u64::from(generation) << 32 | u64::from(row + 1)
}

fn value_row(word: u64) -> Option<u32> {
    (word as u32).checked_sub(1)
}

fn split(fingerprint: Fingerprint) -> [u64; 2] {
    [fingerprint.0 as u64, (fingerprint.0 >> 64) as u64]
}

/// What a hit and verification read of a memo. 40 bytes.
#[repr(C)]
pub(crate) struct MemoHot {
    /// [`Fingerprint`], as two words so the row is 8-byte aligned.
    fingerprint: [u64; 2],
    /// See [`word`]: the only field written after the memo is published.
    value: AtomicU64,
    /// `edge_len` records, in the order they were read.
    edges: NonNull<EdgeRecord>,
    edge_len: u32,
    /// The schema and producer it was made under ([`Store::code`]).
    code: u32,
}

const _: () = assert!(size_of::<MemoHot>() == 40);

// SAFETY: `edges` names records that never change once copied in and stay until the memo is
// freed (its cold row holds their segment, or they are the image's); `value` is atomic.
unsafe impl Send for MemoHot {}
// SAFETY: as above.
unsafe impl Sync for MemoHot {}

impl MemoHot {
    pub(crate) fn fingerprint(&self) -> Fingerprint {
        Fingerprint(u128::from(self.fingerprint[0]) | u128::from(self.fingerprint[1]) << 64)
    }

    pub(crate) fn code(&self) -> u32 {
        self.code
    }
}

/// What only writers read of a memo. 48 bytes.
struct MemoCold {
    key: QueryKey,
    /// The segment `MemoHot::edges` is in, with one hold on it; null for the image's.
    edges: *const Segment,
}

const _: () = assert!(size_of::<MemoCold>() == 48);

// SAFETY: `edges` is only let go of, once, when the row is dropped.
unsafe impl Send for MemoCold {}
// SAFETY: as above.
unsafe impl Sync for MemoCold {}

impl Drop for MemoCold {
    fn drop(&mut self) {
        // SAFETY: the row's one hold on its edges' segment; the row is dropped once no reader
        // can reach the memo.
        unsafe { segment::release(self.edges) };
    }
}

/// Where a value's bytes are. 64 bytes.
struct ValueRow {
    bytes: NonNull<u8>,
    len: usize,
    /// The segment `bytes` is in, with one hold on it; null for a mapping's.
    segment: *const Segment,
    /// A mapped value's content ID, which [`Stored::Disk`] names.
    id: ContentId,
    /// In a segment of this process, which [`Store::evict`] may drop.
    memory: bool,
}

const _: () = assert!(size_of::<ValueRow>() == 64);

// SAFETY: `bytes` never change once copied in or mapped, and stay while the row's hold on its
// segment does, or while the database keeps the mapping.
unsafe impl Send for ValueRow {}
// SAFETY: as above.
unsafe impl Sync for ValueRow {}

impl ValueRow {
    fn new(span: Span, memory: bool, id: ContentId) -> ValueRow {
        let (bytes, len, segment) = span.into_raw();
        ValueRow { bytes, len, segment, id, memory }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: see the `Send` impl: `len` bytes at `bytes`, unchanged while the row lives.
        unsafe { std::slice::from_raw_parts(self.bytes.as_ptr(), self.len) }
    }
}

impl Drop for ValueRow {
    fn drop(&mut self) {
        // SAFETY: the row's one hold on its segment; the row is dropped once no reader can
        // reach it.
        unsafe { segment::release(self.segment) };
    }
}

#[derive(Default)]
struct Codes {
    by: FxMap<(SchemaHash, ContentId), u32>,
    all: Vec<(SchemaHash, ContentId)>,
}

/// One shard of the store's writers: claiming and readying memo and value rows.
#[derive(Default)]
struct Frees {
    memos: Free,
    values: Free,
}

#[repr(align(128))]
#[derive(Default)]
struct Writers(ShortLock<Frees>);

/// Memos on their way out; see the module's comment.
#[derive(Default)]
struct Dying {
    /// Unlinked from their slots, each with the grace begun after.
    unlinked: Vec<(Grace, MemoId)>,
    /// Held by an open view when last checked.
    held: Vec<MemoId>,
    /// Held by no open view when checked, each with the grace begun after the check.
    checked: Vec<(Grace, MemoId)>,
}

/// How many of `held` (in the order their graces were started) have passed their grace.
fn passed<T>(held: &[(Grace, T)]) -> usize {
    match held.last() {
        None => 0,
        Some((newest, _)) if newest.passed() => held.len(),
        Some(_) => held.iter().take_while(|(grace, _)| grace.passed()).count(),
    }
}

/// Every memo of the database and every value of a memo.
///
/// - A memo costs 40 hot bytes, 48 cold bytes and 56 bytes an edge (one range of an edge
///   segment); a value 64 bytes of row, 4 of generation and its bytes (in a value segment,
///   at a 16-byte boundary). Reading a memo's fingerprint, edges or value is one row load
///   by index, and the value one more.
pub(crate) struct Store {
    ids: Ids,
    hot: Rows<MemoHot>,
    cold: Rows<MemoCold>,
    value_ids: Ids,
    values: Rows<ValueRow>,
    generations: Generations,
    writers: Box<[Writers]>,
    mask: usize,
    codes: ShortLock<Codes>,
    dying: ShortLock<Dying>,
    /// Memos a cold write in flight will swap the value of, each with how many: not freed.
    pins: ShortLock<FxMap<MemoId, u32>>,
    // The segments last: the rows above hold spans of them, let go of as they drop.
    edges: Segments,
    bytes: Segments,
    /// Keys' bytes and slots' memo lists.
    keys: Segments,
}

impl Store {
    pub(crate) fn new() -> Store {
        let count = blaze_ids::shard_count();
        Store {
            ids: Ids::new(),
            hot: Rows::new(),
            cold: Rows::new(),
            value_ids: Ids::new(),
            values: Rows::new(),
            generations: Generations::new(),
            writers: (0..count).map(|_| Writers::default()).collect(),
            mask: count - 1,
            codes: ShortLock::default(),
            dying: ShortLock::default(),
            pins: ShortLock::default(),
            edges: Segments::new(16 << 10, 1 << 20, align_of::<EdgeRecord>()),
            bytes: Segments::new(16 << 10, 4 << 20, blaze_format::ALIGN),
            keys: Segments::new(4 << 10, 256 << 10, align_of::<MemoId>()),
        }
    }

    fn writers(&self) -> &ShortLock<Frees> {
        &self.writers[lane() as usize & self.mask].0
    }

    /// The number of a schema and producer pair, the same for every memo of it.
    pub(crate) fn code(&self, schema: SchemaHash, producer: ContentId) -> u32 {
        let mut codes = self.codes.lock();
        if let Some(code) = codes.by.get(&(schema, producer)) {
            return *code;
        }
        let code = u32::try_from(codes.all.len()).expect("2^32 schemas and producers");
        codes.all.push((schema, producer));
        codes.by.insert((schema, producer), code);
        code
    }

    pub(crate) fn parts(&self, code: u32) -> (SchemaHash, ContentId) {
        self.codes.lock().all[code as usize]
    }

    /// Above every memo ID handed out so far.
    pub(crate) fn end(&self) -> u32 {
        self.ids.end()
    }

    fn put_value(&self, frees: &mut Frees, span: Span, memory: bool, id: ContentId) -> u64 {
        let row = self.value_ids.claim(&mut frees.values);
        let generation = self.generations.bump(row);
        // SAFETY: claimed just now and not yet published.
        unsafe { self.values.put(row, ValueRow::new(span, memory, id)) };
        word(generation, row)
    }

    fn add(&self, key: QueryKey, fingerprint: Fingerprint, code: u32, edges: Span, value: Option<(Span, bool, ContentId)>) -> MemoId {
        let mut frees = self.writers().lock();
        let value = match value {
            Some((span, memory, id)) => self.put_value(&mut frees, span, memory, id),
            None => 0,
        };
        let memo = self.ids.claim(&mut frees.memos);
        let (at, len, segment) = edges.into_raw();
        // SAFETY: claimed just now and not yet published.
        unsafe {
            self.hot.put(
                memo,
                MemoHot {
                    fingerprint: split(fingerprint),
                    value: AtomicU64::new(value),
                    edges: at.cast(),
                    edge_len: u32::try_from(len / EDGE_RECORD).expect("2^32 edges of one memo"),
                    code,
                },
            );
            self.cold.put(memo, MemoCold { key, edges: segment });
        }
        memo
    }

    /// A memo a run made: its edges and value copied once into this thread's segments.
    pub(crate) fn make(
        &self,
        key: QueryKey,
        fingerprint: Fingerprint,
        code: u32,
        edges: &[EdgeRecord],
        value: &[u8],
    ) -> MemoId {
        let edges = self.edges.copy(EdgeRecord::as_bytes(edges), align_of::<EdgeRecord>());
        let value = self.bytes.copy(value, blaze_format::ALIGN);
        self.add(key, fingerprint, code, edges, Some((value, true, ContentId::default())))
    }

    /// A memo of the image: its edges and value stay in the mapping.
    ///
    /// # Safety
    ///
    /// `edges` and the value's bytes are in a mapping kept, unchanged, as long as the store
    /// lives ([`Span::mapped`]).
    pub(crate) unsafe fn make_mapped(
        &self,
        key: QueryKey,
        fingerprint: Fingerprint,
        code: u32,
        edges: &[EdgeRecord],
        value: Option<(&[u8], ContentId)>,
    ) -> MemoId {
        // SAFETY: the caller's contract.
        let edges = unsafe { Span::mapped(EdgeRecord::as_bytes(edges)) };
        // SAFETY: as above.
        let value = value.map(|(bytes, id)| (unsafe { Span::mapped(bytes) }, false, id));
        self.add(key, fingerprint, code, edges, value)
    }

    /// A memo's hot row. `memo` was read in the caller's task from a slot or a view's
    /// results, or was made by the caller: memos are freed only once no task that could have
    /// read one is left and no open view holds it (see the module's comment).
    pub(crate) fn hot<'g>(&'g self, _task: &'g Guard, memo: MemoId) -> &'g MemoHot {
        // SAFETY: as the comment says, the memo is not freed while `_task` lives.
        unsafe { self.hot.get(memo) }
    }

    fn cold<'g>(&'g self, _task: &'g Guard, memo: MemoId) -> &'g MemoCold {
        // SAFETY: as in `Store::hot`.
        unsafe { self.cold.get(memo) }
    }

    pub(crate) fn fingerprint(&self, task: &Guard, memo: MemoId) -> Fingerprint {
        self.hot(task, memo).fingerprint()
    }

    pub(crate) fn key(&self, task: &Guard, memo: MemoId) -> QueryKey {
        self.cold(task, memo).key
    }

    /// The memo's edges in the order they were read: one range, read in place.
    pub(crate) fn edges<'g>(&'g self, task: &'g Guard, memo: MemoId) -> &'g [EdgeRecord] {
        let hot = self.hot(task, memo);
        // SAFETY: `edge_len` records written at `edges` before the memo was published, held by
        // its cold row's segment hold (or in the image) until the memo is freed.
        unsafe { std::slice::from_raw_parts(hot.edges.as_ptr(), hot.edge_len as usize) }
    }

    fn value<'g>(&'g self, task: &'g Guard, memo: MemoId) -> Option<(u64, &'g ValueRow)> {
        let word = self.hot(task, memo).value.load(Ordering::Acquire);
        let row = value_row(word)?;
        // SAFETY: a value row is retired only after it is swapped out of its memo's word, and
        // dropped once a grace begun then has passed; this task loaded the word before.
        Some((word, unsafe { self.values.get(row) }))
    }

    /// The memo's value's bytes, while it is in memory or mapped.
    pub(crate) fn bytes<'g>(&'g self, task: &'g Guard, memo: MemoId) -> Option<&'g [u8]> {
        self.value(task, memo).map(|(_, value)| value.bytes())
    }

    /// The value in memory and the word naming it, for a cold write to swap out later.
    pub(crate) fn in_memory<'g>(&'g self, task: &'g Guard, memo: MemoId) -> Option<(u64, &'g [u8])> {
        self.value(task, memo).filter(|(_, value)| value.memory).map(|(word, value)| (word, value.bytes()))
    }

    pub(crate) fn stored(&self, task: &Guard, memo: MemoId) -> Stored {
        match self.value(task, memo) {
            None => Stored::Evicted,
            Some((_, value)) if value.memory => Stored::Memory,
            Some((_, value)) => Stored::Disk(value.id),
        }
    }

    /// Swaps the memo's word from `was` to `now`: the value let go of goes to the limbo, or,
    /// when the word no longer is `was`, `now`'s row (never published) is freed at once.
    fn swap_value(&self, task: &Guard, memo: MemoId, was: u64, now: u64) -> bool {
        let swapped = self.hot(task, memo).value.compare_exchange(was, now, Ordering::AcqRel, Ordering::Acquire);
        let (gone, unlinked) = match swapped {
            Ok(_) => (value_row(was), true),
            Err(_) => (value_row(now), false),
        };
        if let Some(row) = gone {
            let mut frees = self.writers().lock();
            if unlinked {
                frees.values.retire(Grace::start(), row);
            } else {
                // SAFETY: `now` was never published, so no reader has its row.
                unsafe { self.values.drop_row(row) };
                frees.values.ready(row);
            }
        }
        swapped.is_ok()
    }

    /// Takes the memo's value out of memory; a reader still holding it keeps it until its task
    /// ends. `false` when it was not in memory.
    pub(crate) fn evict(&self, task: &Guard, memo: MemoId) -> bool {
        loop {
            let Some((word, value)) = self.value(task, memo) else { return false };
            if !value.memory {
                return false;
            }
            if self.swap_value(task, memo, word, 0) {
                return true;
            }
        }
    }

    /// An evicted value back in memory, copied once into this thread's segment; a racing
    /// refill's wins.
    pub(crate) fn refill(&self, task: &Guard, memo: MemoId, bytes: &[u8]) {
        let span = self.bytes.copy(bytes, blaze_format::ALIGN);
        let now = self.put_value(&mut self.writers().lock(), span, true, ContentId::default());
        self.swap_value(task, memo, 0, now);
    }

    /// The value moved to `bytes` of a mapping the database keeps, if the memo's word is still
    /// `was`: the very value the cold write copied. `false` when it changed since.
    ///
    /// # Safety
    ///
    /// `bytes` are in a mapping kept, unchanged, as long as the store lives.
    pub(crate) unsafe fn to_disk(&self, task: &Guard, memo: MemoId, was: u64, bytes: &[u8], id: ContentId) -> bool {
        // SAFETY: the caller's contract.
        let span = unsafe { Span::mapped(bytes) };
        let now = self.put_value(&mut self.writers().lock(), span, false, id);
        self.swap_value(task, memo, was, now)
    }

    /// Whether memo `a` of this store is memo `b` of `other`: the same fingerprint, schema,
    /// producer and edges.
    pub(crate) fn same(&self, task: &Guard, a: MemoId, other: &Store, b: MemoId) -> bool {
        let (x, y) = (self.hot(task, a), other.hot(task, b));
        let codes = if ptr::eq(self, other) { x.code == y.code } else { self.parts(x.code) == other.parts(y.code) };
        x.fingerprint == y.fingerprint && codes && self.edges(task, a) == other.edges(task, b)
    }

    /// Frees a memo no reader can reach, with its value.
    fn free(&self, memo: MemoId) {
        // SAFETY: the caller's contract: nothing reaches the memo, so its rows and value row
        // are this caller's.
        let word = unsafe { self.hot.get(memo) }.value.load(Ordering::Acquire);
        let mut frees = self.writers().lock();
        if let Some(row) = value_row(word) {
            // SAFETY: the memo's value, reached only through the memo.
            unsafe { self.values.drop_row(row) };
            frees.values.ready(row);
        }
        // SAFETY: as above.
        unsafe {
            self.hot.drop_row(memo);
            self.cold.drop_row(memo);
        }
        frees.memos.ready(memo);
    }

    /// A memo made but never published (an equal one was there already): freed at once.
    pub(crate) fn discard(&self, memo: MemoId) {
        self.free(memo);
    }

    /// Memos just unlinked from their slots.
    pub(crate) fn unlinked(&self, memos: Vec<MemoId>) {
        if memos.is_empty() {
            return;
        }
        let grace = Grace::start();
        self.dying.lock().unlinked.extend(memos.into_iter().map(|memo| (grace, memo)));
    }

    /// The memos to check against the open views, before they are walked: those unlinked
    /// whose grace has passed (no task that read them from a slot is left, so no view can gain
    /// them any more), and those a view held at the last check.
    pub(crate) fn to_check(&self) -> Vec<MemoId> {
        let mut dying = self.dying.lock();
        let mut out = std::mem::take(&mut dying.held);
        let done = passed(&dying.unlinked);
        out.extend(dying.unlinked.drain(..done).map(|(_, memo)| memo));
        out
    }

    /// After the open views were walked: those `held` says a view holds wait for the next
    /// check, and the rest are freed once a grace begun now has passed (a task in a view closed
    /// before the walk may still hold one).
    pub(crate) fn checked(&self, memos: Vec<MemoId>, held: impl Fn(MemoId) -> bool) {
        if memos.is_empty() {
            return;
        }
        let grace = Grace::start();
        let mut dying = self.dying.lock();
        for memo in memos {
            if held(memo) {
                dying.held.push(memo);
            } else {
                dying.checked.push((grace, memo));
            }
        }
    }

    /// Keeps `memos` from being freed until [`Store::unpin`]: a cold write will swap their
    /// values.
    pub(crate) fn pin(&self, memos: impl IntoIterator<Item = MemoId>) {
        let mut pins = self.pins.lock();
        for memo in memos {
            *pins.entry(memo).or_default() += 1;
        }
    }

    pub(crate) fn unpin(&self, memo: MemoId) {
        let mut pins = self.pins.lock();
        if let Some(count) = pins.get_mut(&memo) {
            *count -= 1;
            if *count == 0 {
                pins.remove(&memo);
            }
        }
    }

    /// Frees the checked memos whose grace has passed, the values let go of whose grace has
    /// passed, and the segments left with nothing in them.
    pub(crate) fn collect(&self) {
        let ready: Vec<MemoId> = {
            let mut dying = self.dying.lock();
            let done = passed(&dying.checked);
            dying.checked.drain(..done).map(|(_, memo)| memo).collect()
        };
        if !ready.is_empty() {
            let (pinned, free): (Vec<MemoId>, Vec<MemoId>) = {
                let pins = self.pins.lock();
                ready.into_iter().partition(|memo| pins.contains_key(memo))
            };
            for memo in free {
                self.free(memo);
            }
            if !pinned.is_empty() {
                let grace = Grace::start();
                self.dying.lock().checked.extend(pinned.into_iter().map(|memo| (grace, memo)));
            }
        }
        let values = &self.values;
        for writers in self.writers.iter() {
            // SAFETY: a retired value row's grace has passed: no task that could reach it is left.
            writers.0.lock().values.reclaim(|row| unsafe { values.drop_row(row) });
        }
        self.edges.collect();
        self.bytes.collect();
        self.keys.collect();
    }

    /// How many values let go of wait for their grace, after freeing those past it.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        let values = &self.values;
        self.writers
            .iter()
            .map(|writers| {
                let mut frees = writers.0.lock();
                // SAFETY: as in `Store::collect`.
                frees.values.reclaim(|row| unsafe { values.drop_row(row) });
                frees.values.retired()
            })
            .sum()
    }

    /// How many memos let go of are not freed yet.
    #[cfg(test)]
    pub(crate) fn dying(&self) -> usize {
        let dying = self.dying.lock();
        dying.unlinked.len() + dying.held.len() + dying.checked.len()
    }

    #[cfg(test)]
    pub(crate) fn segments(&self) -> (usize, usize, usize) {
        (self.edges.count(), self.bytes.count(), self.keys.count())
    }

    /// Frees everything let go of that no open view holds (`held`) and no cold write pins:
    /// with `&mut`, no task reads the store.
    pub(crate) fn reclaim(&mut self, held: impl Fn(MemoId) -> bool) {
        let dying = std::mem::take(self.dying.get_mut());
        let pinned: Vec<MemoId> = self.pins.get_mut().keys().copied().collect();
        let mut keep = Dying::default();
        let all = dying
            .unlinked
            .into_iter()
            .map(|(_, memo)| memo)
            .chain(dying.held)
            .chain(dying.checked.into_iter().map(|(_, memo)| memo));
        for memo in all {
            if held(memo) {
                keep.held.push(memo);
            } else if pinned.contains(&memo) {
                keep.checked.push((Grace::start(), memo));
            } else {
                self.free(memo);
            }
        }
        *self.dying.get_mut() = keep;
        let values = &self.values;
        for writers in self.writers.iter_mut() {
            // SAFETY: `&mut` excludes every reader.
            writers.0.get_mut().values.reclaim_all(|row| unsafe { values.drop_row(row) });
        }
        self.edges.collect();
        self.bytes.collect();
        self.keys.collect();
    }

    /// `bytes` as a key's, copied once into this thread's key segment.
    pub(crate) fn key_bytes(&self, bytes: &[u8]) -> Span {
        self.keys.copy(bytes, 1)
    }

    /// A slot of `memos` (the newest last) and `key`: up to [`INLINE`] memos in the slot
    /// itself, more in a list copied into this thread's key segment.
    pub(crate) fn slot(&self, key: Option<Span>, memos: &[MemoId]) -> Slot {
        let len = u32::try_from(memos.len()).expect("2^32 memos of one key");
        if memos.len() <= INLINE {
            let mut inline = [0; INLINE];
            inline[..memos.len()].copy_from_slice(memos);
            return Slot { key, list: None, len, inline };
        }
        // SAFETY: `u32`s have no padding: every byte of the slice is initialized.
        let bytes = unsafe { std::slice::from_raw_parts(memos.as_ptr().cast::<u8>(), size_of_val(memos)) };
        Slot { key, list: Some(self.keys.copy(bytes, align_of::<MemoId>())), len, inline: [0; INLINE] }
    }
}

/// How many memo IDs a [`Slot`] holds in itself.
const INLINE: usize = 3;

/// What the database keeps of one key: its memos (the newest last) and its key's bytes, so it
/// can run it again by its [`QueryKey`] alone. A changed slot is a new one swapped in. 64
/// bytes; a key of more than [`INLINE`] memos adds 4 bytes a memo in a list.
pub(crate) struct Slot {
    key: Option<Span>,
    /// All the memo IDs, when there are more than [`INLINE`]: `u32`s at a 4-byte boundary.
    list: Option<Span>,
    len: u32,
    inline: [MemoId; INLINE],
}

const _: () = assert!(size_of::<Slot>() == 64);

impl Slot {
    pub(crate) fn empty() -> Slot {
        Slot { key: None, list: None, len: 0, inline: [0; INLINE] }
    }

    pub(crate) fn memos(&self) -> &[MemoId] {
        match &self.list {
            Some(list) => {
                let bytes = list.bytes();
                // SAFETY: `len` `u32`s copied in at a 4-byte boundary by `Store::slot`.
                unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<MemoId>(), self.len as usize) }
            }
            None => &self.inline[..self.len as usize],
        }
    }

    pub(crate) fn key(&self) -> Option<&[u8]> {
        self.key.as_ref().map(Span::bytes)
    }

    /// The key's bytes, held once more for another slot.
    pub(crate) fn share_key(&self) -> Option<Span> {
        self.key.as_ref().map(Span::share)
    }
}

#[cfg(test)]
mod tests;
