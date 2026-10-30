//! The keys' records (docs/v2/plan/lanes/fastcompile/sched.md, "The table"): what each task
//! key's last runs took and held, in a table of fixed size that one writer changes and any
//! thread reads with no lock and no write to shared memory.
//!
//! **Layout.** A record is 64 bytes, one cache line, in buckets of [`WAYS`]. A key's bucket is
//! its own first eight bytes (it is a content hash already) mixed with its kind and masked, so
//! a lookup hashes nothing and reads one bucket: four lines, two of the pairs that arm64 and
//! x86 prefetch together. A bucket that is full when a new key comes drops its oldest record by
//! the writer's clock (a count of observations, not time), so the table holds the keys seen
//! most lately, never more than its fixed size, and the same observations leave the same
//! records in the same slots.
//!
//! **Reads.** Each record is a seqlock over atomics: the writer makes the record's sequence
//! odd, stores the fields relaxed after a release fence, and makes the sequence even again with
//! a release store; a reader loads the sequence with acquire, the fields relaxed, and the
//! sequence again after an acquire fence, and keeps the fields only when both loads gave the
//! same even number. A read writes nothing shared, so readers on every core scale; one that met
//! a write under way reads again. A table has one [`Writer`], which is not `Sync`, so no two
//! writes race, and the writer reads its own records with plain relaxed loads.

use std::{
    cell::Cell,
    hint,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{
            AtomicU64,
            Ordering::{Acquire, Relaxed, Release},
            fence,
        },
    },
};

use blaze_graph::{Key, admission::TaskKind};

/// Records a bucket holds.
pub const WAYS: usize = 4;
/// Runs a record keeps the time of.
pub const RUNS: usize = 4;

/// One key's record, as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub kind: TaskKind,
    /// The key's first 16 bytes. Two keys of a kind that share them share a record: a wrong
    /// prediction costs only an order or an admission, never a result.
    pub key: [u64; 2],
    /// Its runs seen, saturating.
    pub seen: u32,
    /// The writer's clock at its last run: its age.
    pub stamp: u64,
    /// The peak its next run is predicted to hold, in bytes: a peak at or over it becomes it at
    /// once, and one under it brings it half the way down.
    pub peak: u64,
    /// Its last runs' times in microseconds (saturating at 71 minutes), a ring: run `n`
    /// (counting from 0) at `n % RUNS`; the slots no run reached yet are 0.
    pub runs: [u32; RUNS],
}

impl Record {
    /// The first run of `kind`'s `key`, at `stamp`.
    pub fn first(kind: TaskKind, key: [u64; 2], stamp: u64, ran_us: u32, peak: u64) -> Record {
        let mut runs = [0; RUNS];
        runs[0] = ran_us;
        Record { kind, key, seen: 1, stamp, peak, runs }
    }

    /// Another run of it, at `stamp`, that took `ran_us` and held `peak` bytes at most.
    pub fn observe(&mut self, stamp: u64, ran_us: u32, peak: u64) {
        self.runs[self.seen as usize % RUNS] = ran_us;
        self.seen = self.seen.saturating_add(1);
        self.stamp = stamp;
        self.peak = if peak >= self.peak { peak } else { self.peak - (self.peak - peak) / 2 };
    }

    /// The time its next run is predicted to take, in microseconds: the median of its last
    /// runs, the middle two's mean rounded up for an even count, so one slow or fast run
    /// moves it little.
    pub fn ran_us(&self) -> u64 {
        let count = (self.seen as usize).min(RUNS);
        let mut runs = self.runs;
        let last = &mut runs[..count];
        last.sort_unstable();
        match count {
            0 => 0,
            _ if count % 2 == 1 => u64::from(last[count / 2]),
            _ => (u64::from(last[count / 2 - 1]) + u64::from(last[count / 2])).div_ceil(2),
        }
    }

    /// Whether observations make it: seen, stamped, and no run time where no run reached.
    pub(crate) fn is_sound(&self) -> bool {
        let filled = (self.seen as usize).min(RUNS);
        self.seen > 0 && self.stamp > 0 && self.runs[filled..].iter().all(|&it| it == 0)
    }
}

/// A record's fields as words: `kind << 32 | seen`, the stamp, the key's halves, the peak, and
/// the runs two to a word, the lower first.
type Words = [u64; 7];

fn pack(record: &Record) -> Words {
    let runs = record.runs.map(u64::from);
    [
        (u64::from(record.kind.0) << 32) | u64::from(record.seen),
        record.stamp,
        record.key[0],
        record.key[1],
        record.peak,
        runs[0] | (runs[1] << 32),
        runs[2] | (runs[3] << 32),
    ]
}

/// The record of `words`; `None` for an empty slot (`seen` 0).
fn unpack(words: Words) -> Option<Record> {
    let seen = words[0] as u32;
    (seen != 0).then(|| Record {
        kind: TaskKind((words[0] >> 32) as u32),
        seen,
        stamp: words[1],
        key: [words[2], words[3]],
        peak: words[4],
        runs: [words[5] as u32, (words[5] >> 32) as u32, words[6] as u32, (words[6] >> 32) as u32],
    })
}

/// One record's slot: its sequence and its seven words. 64 bytes.
#[repr(C, align(64))]
struct Slot {
    /// Odd while a write is under way.
    seq: AtomicU64,
    words: [AtomicU64; 7],
}

const _: () = assert!(size_of::<Slot>() == 64);

impl Slot {
    fn empty() -> Slot {
        Slot { seq: AtomicU64::new(0), words: std::array::from_fn(|_| AtomicU64::new(0)) }
    }

    fn load(&self) -> Words {
        std::array::from_fn(|at| self.words[at].load(Relaxed))
    }

    /// Its record as the writer reads it: no write races the loads.
    fn peek(&self) -> Option<Record> {
        unpack(self.load())
    }

    /// Its record as any thread reads it: a whole one, never one torn by a write under way.
    fn read(&self) -> Option<Record> {
        loop {
            let before = self.seq.load(Acquire);
            if before & 1 == 0 {
                let words = self.load();
                fence(Acquire);
                if self.seq.load(Relaxed) == before {
                    return unpack(words);
                }
            }
            hint::spin_loop();
        }
    }

    /// Puts `record` in it. Only the table's one writer calls it.
    fn write(&self, record: &Record) {
        let seq = self.seq.load(Relaxed);
        self.seq.store(seq.wrapping_add(1), Relaxed);
        fence(Release);
        for (word, value) in self.words.iter().zip(pack(record)) {
            word.store(value, Relaxed);
        }
        self.seq.store(seq.wrapping_add(2), Release);
    }
}

/// The slots, `buckets * WAYS` of them: 64 bytes a key, a lookup one bucket.
struct Table {
    slots: Box<[Slot]>,
    /// The bucket count less one, a power of two less one.
    mask: u64,
}

/// The first slot of the bucket of `kind`'s `key` in a table of `mask + 1` buckets.
fn bucket(mask: u64, kind: TaskKind, key: [u64; 2]) -> usize {
    ((key[0] ^ u64::from(kind.0).wrapping_mul(0x9e37_79b9_7f4a_7c15)) & mask) as usize * WAYS
}

/// The first slot of the bucket of `kind`'s `key` in a table of `slots` slots.
pub(crate) fn bucket_in(slots: usize, kind: TaskKind, key: [u64; 2]) -> usize {
    bucket((slots / WAYS - 1) as u64, kind, key)
}

/// A task key's first 16 bytes, as two words.
pub fn halves(key: &Key) -> [u64; 2] {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, ..] = key.0;
    [u64::from_le_bytes([a, b, c, d, e, f, g, h]), u64::from_le_bytes([i, j, k, l, m, n, o, p])]
}

impl Table {
    fn new(slots: usize) -> Table {
        assert!(
            slots.is_power_of_two() && slots >= WAYS,
            "a history's table has a power of two slots, at least {WAYS}, not {slots}"
        );
        Table {
            slots: std::iter::repeat_with(Slot::empty).take(slots).collect(),
            mask: (slots / WAYS - 1) as u64,
        }
    }

    fn bucket(&self, kind: TaskKind, key: [u64; 2]) -> usize {
        bucket(self.mask, kind, key)
    }

    /// The slot of `kind`'s `key` and its record, each slot of its bucket read by `read`.
    fn find(
        &self,
        kind: TaskKind,
        key: [u64; 2],
        read: impl Fn(&Slot) -> Option<Record>,
    ) -> Option<(usize, Record)> {
        let first = self.bucket(kind, key);
        (first..first + WAYS).find_map(|at| {
            read(&self.slots[at]).filter(|it| it.kind == kind && it.key == key).map(|it| (at, it))
        })
    }
}

/// The one writer of a table: it observes runs, and gives [`Reader`]s to other threads.
pub struct Writer {
    table: Arc<Table>,
    clock: u64,
    evicted: u64,
    /// Not `Sync`: one thread writes at a time, so no two writes to a record race.
    _one: PhantomData<Cell<()>>,
}

impl Writer {
    /// An empty table of `slots` records (a power of two, at least [`WAYS`]).
    ///
    /// # Panics
    ///
    /// When `slots` is not such a number.
    pub fn new(slots: usize) -> Writer {
        Writer { table: Arc::new(Table::new(slots)), clock: 0, evicted: 0, _one: PhantomData }
    }

    pub fn slots(&self) -> usize {
        self.table.slots.len()
    }

    /// The observations made: the stamp of the last.
    pub fn clock(&self) -> u64 {
        self.clock
    }

    /// The records dropped for a newer key of their bucket.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// A reader of the same table, for any thread: one reference count, taken once.
    pub fn reader(&self) -> Reader {
        Reader { table: self.table.clone() }
    }

    /// The record of `kind`'s `key`, if the table holds it.
    pub fn get(&self, kind: TaskKind, key: &Key) -> Option<Record> {
        self.table.find(kind, halves(key), Slot::peek).map(|it| it.1)
    }

    /// A run of `kind`'s `key` took `ran_us` and held `peak` bytes at most: its record learns
    /// it, or a new one takes the bucket's first empty slot, else its oldest.
    pub fn observe(&mut self, kind: TaskKind, key: &Key, ran_us: u32, peak: u64) {
        self.clock += 1;
        let stamp = self.clock;
        let key = halves(key);
        let first = self.table.bucket(kind, key);
        let mut free = None;
        let (mut oldest, mut oldest_at) = (u64::MAX, first);
        for at in first..first + WAYS {
            match self.table.slots[at].peek() {
                Some(mut record) if record.kind == kind && record.key == key => {
                    record.observe(stamp, ran_us, peak);
                    self.table.slots[at].write(&record);
                    return;
                }
                Some(record) if record.stamp < oldest => (oldest, oldest_at) = (record.stamp, at),
                Some(_) => {}
                None => _ = free.get_or_insert(at),
            }
        }
        let at = free.unwrap_or_else(|| {
            self.evicted += 1;
            oldest_at
        });
        self.table.slots[at].write(&Record::first(kind, key, stamp, ran_us, peak));
    }

    /// Every record and its slot, in slot order.
    pub fn records(&self) -> impl Iterator<Item = (usize, Record)> + '_ {
        self.table.slots.iter().enumerate().filter_map(|(at, slot)| Some((at, slot.peek()?)))
    }

    /// The first slot of the bucket `record` belongs in.
    pub(crate) fn bucket_of(&self, record: &Record) -> usize {
        self.table.bucket(record.kind, record.key)
    }

    /// Puts `record`, stamp and all, in slot `at` of its bucket: a table read back.
    pub(crate) fn put_at(&mut self, at: usize, record: &Record) {
        debug_assert_eq!(at - at % WAYS, self.bucket_of(record));
        self.table.slots[at].write(record);
    }

    /// Puts `record`, stamp and all, where a new key of it would go: a table read back into
    /// another size, its records given oldest first.
    pub(crate) fn place(&mut self, record: &Record) {
        let first = self.bucket_of(record);
        let mut free = None;
        let (mut oldest, mut oldest_at) = (u64::MAX, first);
        for at in first..first + WAYS {
            match self.table.slots[at].peek() {
                Some(it) if it.stamp < oldest => (oldest, oldest_at) = (it.stamp, at),
                Some(_) => {}
                None => _ = free.get_or_insert(at),
            }
        }
        let at = free.unwrap_or_else(|| {
            self.evicted += 1;
            oldest_at
        });
        self.table.slots[at].write(record);
    }

    /// Its clock and count of records dropped, as a table read back had them.
    pub(crate) fn set_counts(&mut self, clock: u64, evicted: u64) {
        self.clock = clock;
        self.evicted = evicted;
    }
}

/// Reads a table from any thread, with no lock and no write to shared memory.
#[derive(Clone)]
pub struct Reader {
    table: Arc<Table>,
}

impl Reader {
    /// The record of `kind`'s `key`, if the table holds it: a whole one, as some write left it.
    pub fn get(&self, kind: TaskKind, key: &Key) -> Option<Record> {
        self.table.find(kind, halves(key), Slot::read).map(|it| it.1)
    }
}
