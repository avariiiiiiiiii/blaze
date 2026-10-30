#![cfg(all(feature = "inventory", not(feature = "shuttle")))]

//! Lock-free interning (`src/interned.rs`, `src/interned/table.rs`): seeded intern sequences
//! across revisions give the IDs and generations a model of the reuse sweep predicts, immortal
//! and reusable, and threads racing to intern overlapping keys get one ID per key.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use blaze_ids::rng::SplitMix64;
use salsa::plumbing::AsId;
use salsa::{Database, DatabaseImpl, Setter};

#[salsa::input]
struct Keys {
    #[returns(ref)]
    keys: Vec<u32>,
}

/// One hash for every key: all values land in one shard and one probe chain, so every lookup
/// meets every other key's value under its own hash, stale ones included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, salsa::SalsaValue)]
struct SameHash(u32);

impl Hash for SameHash {
    fn hash<H: Hasher>(&self, state: &mut H) {
        0u8.hash(state);
    }
}

#[salsa::interned(revisions = 3)]
struct Reusable<'db> {
    #[returns(copy)]
    key: SameHash,
}

#[salsa::interned(revisions = usize::MAX)]
struct Immortal<'db> {
    #[returns(copy)]
    key: SameHash,
}

#[salsa::interned(revisions = usize::MAX)]
struct Spread<'db> {
    #[returns(copy)]
    key: u32,
}

/// The `(index, generation)` of each key's value, in order.
#[salsa::tracked(returns(ref))]
fn intern_reusable(db: &dyn Database, keys: Keys) -> Vec<(u32, u32)> {
    keys.keys(db)
        .iter()
        .map(|&key| {
            let value = Reusable::new(db, SameHash(key));
            assert_eq!(value.key(db), SameHash(key));
            let id = value.as_id();
            (id.index(), id.generation())
        })
        .collect()
}

#[salsa::tracked(returns(ref))]
fn intern_immortal(db: &dyn Database, keys: Keys) -> Vec<(u32, u32)> {
    keys.keys(db)
        .iter()
        .map(|&key| {
            let value = Immortal::new(db, SameHash(key));
            assert_eq!(value.key(db), SameHash(key));
            let id = value.as_id();
            (id.index(), id.generation())
        })
        .collect()
}

#[salsa::tracked(returns(ref))]
fn intern_spread(db: &dyn Database, keys: Keys) -> Vec<(u32, u32)> {
    keys.keys(db)
        .iter()
        .map(|&key| {
            let value = Spread::new(db, key);
            assert_eq!(value.key(db), key);
            let id = value.as_id();
            (id.index(), id.generation())
        })
        .collect()
}

/// One to five keys below `range`.
fn some_keys(rng: &mut SplitMix64, range: u64) -> Vec<u32> {
    let len = 1 + rng.below(5);
    (0..len).map(|_| rng.below(range) as u32).collect()
}

/// The reuse sweep of one shard as `interned.rs` documents it: values in allocation order, a
/// cursor, and up to eight looked at per miss for one not interned in the last `revisions`
/// revisions anything was interned in.
struct Model {
    /// Those revisions, most recent first; the start revision (1) until recorded.
    queue: Vec<u64>,
    slots: Vec<Slot>,
    by_key: HashMap<u32, usize>,
    cursor: usize,
}

struct Slot {
    key: u32,
    generation: u32,
    last: u64,
}

impl Model {
    fn new(revisions: usize) -> Self {
        Self {
            queue: vec![1; revisions],
            slots: Vec::new(),
            by_key: HashMap::new(),
            cursor: 0,
        }
    }

    fn oldest(&self) -> u64 {
        *self.queue.last().unwrap()
    }

    /// The slot and generation `key` gets in revision `current`.
    fn intern(&mut self, key: u32, current: u64) -> (usize, u32) {
        if self.queue[0] < current {
            self.queue.rotate_right(1);
            self.queue[0] = current;
        }
        if let Some(&at) = self.by_key.get(&key) {
            let slot = &mut self.slots[at];
            slot.last = slot.last.max(current);
            return (at, slot.generation);
        }
        if self.oldest() > 1 {
            let len = self.slots.len();
            for _ in 0..len.min(8) {
                let at = self.cursor;
                self.cursor = if at + 1 == len { 0 } else { at + 1 };
                if self.slots[at].last < self.oldest() {
                    let slot = &mut self.slots[at];
                    self.by_key.remove(&slot.key);
                    slot.key = key;
                    slot.generation += 1;
                    slot.last = current;
                    self.by_key.insert(key, at);
                    return (at, slot.generation);
                }
            }
        }
        self.slots.push(Slot {
            key,
            generation: 0,
            last: current,
        });
        self.by_key.insert(key, self.slots.len() - 1);
        (self.slots.len() - 1, 0)
    }
}

/// Seeded keys interned by a query in each of many revisions: every value's slot and generation
/// is the model's, though every key shares one hash and so every lookup pins and compares the
/// stale values of other keys.
#[test]
fn reusable_ids_match_the_sweep_model() {
    let mut reused = 0;
    for seed in 0..8 {
        let mut rng = SplitMix64::new(seed);
        let mut db = DatabaseImpl::new();
        let keys = Keys::new(&db, some_keys(&mut rng, 10));
        let mut model = Model::new(3);
        // The table index of each model slot, learned when the model allocates it.
        let mut index_of: Vec<u32> = Vec::new();
        let mut revision = 1;
        for step in 0..300 {
            if step > 0 {
                keys.set_keys(&mut db).to(some_keys(&mut rng, 10));
                revision += 1;
            }
            let wanted = keys.keys(&db).clone();
            let got = intern_reusable(&db, keys).clone();
            assert_eq!(got.len(), wanted.len());
            for (&key, &(index, generation)) in wanted.iter().zip(&got) {
                let (at, model_generation) = model.intern(key, revision);
                if at == index_of.len() {
                    assert!(
                        !index_of.contains(&index),
                        "seed {seed} step {step}: key {key} allocated an index in use"
                    );
                    index_of.push(index);
                }
                assert_eq!(
                    (index, generation),
                    (index_of[at], model_generation),
                    "seed {seed} step {step} key {key}"
                );
            }
        }
        reused += model.slots.iter().filter(|slot| slot.generation > 0).count();
    }
    assert!(reused > 0, "the sequences reused slots");
}

/// Immortal values, colliding in one shard or spread over all: one slot per key for good, at
/// generation zero, across revisions.
#[test]
fn immortal_ids_are_stable() {
    for seed in 0..4 {
        let mut rng = SplitMix64::new(100 + seed);
        let mut db = DatabaseImpl::new();
        let keys = Keys::new(&db, some_keys(&mut rng, 40));
        let mut colliding: HashMap<u32, u32> = HashMap::new();
        let mut spread: HashMap<u32, u32> = HashMap::new();
        for step in 0..200 {
            if step > 0 {
                keys.set_keys(&mut db).to(some_keys(&mut rng, 40));
            }
            let wanted = keys.keys(&db).clone();
            for (got, seen) in [
                (intern_immortal(&db, keys).clone(), &mut colliding),
                (intern_spread(&db, keys).clone(), &mut spread),
            ] {
                for (&key, &(index, generation)) in wanted.iter().zip(&got) {
                    assert_eq!(generation, 0, "seed {seed} step {step} key {key}");
                    if !seen.contains_key(&key) {
                        assert!(
                            !seen.values().any(|&it| it == index),
                            "seed {seed} step {step}: key {key} allocated an index in use"
                        );
                    }
                    assert_eq!(*seen.entry(key).or_insert(index), index);
                }
            }
        }
    }
}

/// Keys for thread `thread` in `round`: overlapping with every other thread's.
fn thread_keys(seed: u64, thread: u64, round: u64) -> Vec<u32> {
    let mut rng = SplitMix64::new(seed * 1_000 + round * 10 + thread);
    (0..6).map(|_| rng.below(12) as u32).collect()
}

/// Eight threads interning overlapping keys from queries, in revisions where many of the values
/// are stale (pinned by readers, claimed by sweeps), get one ID per key, each holding its key.
#[test]
fn racing_threads_get_one_id_per_key() {
    for seed in 0..4 {
        let mut db = DatabaseImpl::new();
        let inputs: Vec<Keys> = (0..8)
            .map(|thread| Keys::new(&db, thread_keys(seed, thread, 0)))
            .collect();
        for round in 0..8 {
            for (thread, input) in inputs.iter().enumerate() {
                input
                    .set_keys(&mut db)
                    .to(thread_keys(seed, thread as u64, round));
            }
            let db = &db;
            let results: Vec<(Vec<u32>, Vec<(u32, u32)>)> = std::thread::scope(|scope| {
                let workers: Vec<_> = inputs
                    .iter()
                    .map(|&input| {
                        scope.spawn(move || {
                            let ids = intern_reusable(db, input).clone();
                            (input.keys(db).clone(), ids)
                        })
                    })
                    .collect();
                workers.into_iter().map(|it| it.join().unwrap()).collect()
            });
            let mut one: HashMap<u32, (u32, u32)> = HashMap::new();
            for (keys, ids) in results {
                for (key, id) in keys.into_iter().zip(ids) {
                    assert_eq!(
                        *one.entry(key).or_insert(id),
                        id,
                        "seed {seed} round {round} key {key}"
                    );
                }
            }
            let mut slots: HashMap<u32, u32> = HashMap::new();
            for (&key, &(index, _)) in &one {
                assert!(
                    slots.insert(index, key).is_none(),
                    "seed {seed} round {round}: two live keys share slot {index}"
                );
            }
        }

        // Outside queries too, where values are interned for good.
        let db = &db;
        let found: Vec<Vec<(u32, salsa::Id)>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8u64)
                .map(|thread| {
                    scope.spawn(move || {
                        let mut rng = SplitMix64::new(seed * 77 + thread);
                        (0..2_000)
                            .map(|_| {
                                let key = rng.below(500) as u32;
                                let value = Spread::new(db, key);
                                assert_eq!(value.key(db), key);
                                (key, value.as_id())
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        let mut one: HashMap<u32, salsa::Id> = HashMap::new();
        for (key, id) in found.into_iter().flatten() {
            assert_eq!(*one.entry(key).or_insert(id), id, "seed {seed} key {key}");
        }
    }
}
