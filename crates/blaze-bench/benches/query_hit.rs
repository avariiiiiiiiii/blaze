//! The front end's query database on its hot paths, the same query shapes on each engine
//! (docs/v2/plan/lanes/db/query-unify.md, "How it is measured"; the numbers are in
//! docs/v2/plan/lanes/db/speed-of-light.md, "Measured: salsa fork vs blaze_query::db"):
//!
//! - `hit/*`: a warm hit of a function of one interned ID returning `u64`, over [`KEYS`] keys,
//!   in order and scattered, one hit an iteration;
//! - `hit_in_query/*`: the same [`KEYS`] hits from inside a query, with the edge each one
//!   records: an iteration is a write to the outer query's input (of a lower durability than
//!   the keys) and its run, so divide by [`KEYS`] for a hit;
//! - `verify/*`: a write to an unrelated input, then every one of [`READERS`] readers of
//!   [`EDGES`] edges each, over a pool of [`POOL`] leaves, fetched again; `other_durability`
//!   writes below the readers' durability, `same_durability` at it, so each reader verifies
//!   its edges. Divide by [`READERS`] × [`EDGES`] for an edge;
//! - `interned/hit`: interning a value already interned, scattered over [`KEYS`];
//! - `cycle/fixpoint`: a write to the input of a cycle of [`CYCLE`] queries, then the cycle run
//!   again to its fixpoint from `cycle_initial` (two iterations).
//!
//! Every benchmark does the same work on every run, so that tango can pair the binary of a
//! change with the binary of its base.

use std::hint::black_box;

use tango_bench::{IntoBenchmarks, benchmark_fn, tango_benchmarks};

const KEYS: usize = 65_536;
const POOL: usize = 4096;
const READERS: usize = 16_384;
const EDGES: usize = 16;
const CYCLE: usize = 8;

/// The `i`th key of a walk over [`KEYS`]: in order, or scattered by an odd stride (a
/// permutation, since [`KEYS`] is a power of two).
fn walk(i: usize, scattered: bool) -> usize {
    if scattered { (i * 40_503) % KEYS } else { i }
}

/// The leaves the `r`th reader reads.
fn pick(r: usize, e: usize) -> usize {
    (r * 2_654_435_761 + e * 40_503) % POOL
}

mod fork {
    use salsa::{Durability, Setter};

    use super::{CYCLE, EDGES, KEYS, POOL, READERS, pick, walk};

    #[salsa::db]
    #[derive(Default)]
    pub(crate) struct Db {
        storage: salsa::Storage<Self>,
    }

    #[salsa::db]
    impl salsa::Database for Db {}

    #[salsa::interned(unsafe(no_lifetime), revisions = usize::MAX)]
    pub(crate) struct Key {
        #[returns(copy)]
        value: u32,
    }

    #[salsa::tracked(returns(copy))]
    #[inline(never)]
    pub(crate) fn double(db: &Db, key: Key) -> u64 {
        u64::from(key.value(db)) * 2
    }

    /// Interned keys `0..KEYS` and the order a walk takes them in, every hit warm.
    pub(crate) struct Hits {
        pub(crate) db: Db,
        pub(crate) order: Vec<Key>,
    }

    impl Hits {
        pub(crate) fn new(scattered: bool) -> Hits {
            let db = Db::default();
            let keys: Vec<Key> = (0..KEYS).map(|i| Key::new(&db, i as u32)).collect();
            let order: Vec<Key> = (0..KEYS).map(|i| keys[walk(i, scattered)]).collect();
            for &key in &order {
                std::hint::black_box(double(&db, key));
            }
            Hits { db, order }
        }
    }

    #[salsa::input]
    pub(crate) struct Sweep {
        #[returns(copy)]
        tick: u32,
        #[returns(ref)]
        order: Vec<Key>,
    }

    #[salsa::tracked(returns(copy))]
    #[inline(never)]
    pub(crate) fn sweep(db: &Db, sweep: Sweep) -> u64 {
        let tick = u64::from(sweep.tick(db));
        sweep.order(db).iter().map(|&key| double(db, key)).sum::<u64>() + tick
    }

    /// [`Hits`] behind an input of low durability that a query walks; the keys are interned
    /// outside any query, so their hits are of the highest durability.
    pub(crate) struct InQuery {
        pub(crate) db: Db,
        pub(crate) input: Sweep,
        pub(crate) tick: u32,
    }

    impl InQuery {
        pub(crate) fn new(scattered: bool) -> InQuery {
            let Hits { db, order } = Hits::new(scattered);
            let input = Sweep::builder(0, order).durability(Durability::LOW).new(&db);
            std::hint::black_box(sweep(&db, input));
            InQuery { db, input, tick: 0 }
        }

        pub(crate) fn run(&mut self) -> u64 {
            self.tick ^= 1;
            self.input.set_tick(&mut self.db).to(self.tick);
            sweep(&self.db, self.input)
        }
    }

    #[salsa::input]
    pub(crate) struct Leaf {
        #[returns(copy)]
        value: u32,
    }

    #[salsa::input]
    pub(crate) struct Reader {
        #[returns(ref)]
        picks: Vec<Leaf>,
    }

    #[salsa::input]
    pub(crate) struct Other {
        #[returns(copy)]
        value: u32,
    }

    #[salsa::tracked(returns(copy))]
    #[inline(never)]
    fn leaf(db: &Db, input: Leaf) -> u32 {
        input.value(db).wrapping_mul(3)
    }

    #[salsa::tracked(returns(copy))]
    #[inline(never)]
    fn read_all(db: &Db, reader: Reader) -> u64 {
        reader.picks(db).iter().map(|&input| u64::from(leaf(db, input))).sum()
    }

    /// Readers and leaves of medium durability, verified, and an input none of them reads.
    pub(crate) struct Verify {
        db: Db,
        readers: Vec<Reader>,
        other: Other,
        durability: Durability,
        tick: u32,
        pub(crate) expected: u64,
    }

    impl Verify {
        /// `same`: the unrelated input is of the readers' durability, else of a lower one.
        pub(crate) fn new(same: bool) -> Verify {
            let db = Db::default();
            let pool: Vec<Leaf> = (0..POOL)
                .map(|i| Leaf::builder(i as u32).durability(Durability::MEDIUM).new(&db))
                .collect();
            let readers: Vec<Reader> = (0..READERS)
                .map(|r| {
                    let picks = (0..EDGES).map(|e| pool[pick(r, e)]).collect();
                    Reader::builder(picks).durability(Durability::MEDIUM).new(&db)
                })
                .collect();
            let durability = if same { Durability::MEDIUM } else { Durability::LOW };
            let other = Other::builder(0).durability(durability).new(&db);
            let expected = readers.iter().map(|&reader| read_all(&db, reader)).sum();
            Verify { db, readers, other, durability, tick: 0, expected }
        }

        pub(crate) fn run(&mut self) -> u64 {
            self.tick ^= 1;
            self.other.set_value(&mut self.db).with_durability(self.durability).to(self.tick);
            self.readers.iter().map(|&reader| read_all(&self.db, reader)).sum()
        }
    }

    #[salsa::input]
    pub(crate) struct Seed {
        #[returns(copy)]
        cap: u32,
    }

    fn initial(_db: &Db, _id: salsa::Id, _seed: Seed) -> u32 {
        0
    }

    fn step(db: &Db, seed: Seed, next: u32) -> u32 {
        (next + 1).min(seed.cap(db))
    }

    #[salsa::tracked(returns(copy), cycle_initial = initial)]
    pub(crate) fn c0(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c1(db, seed))
    }

    #[salsa::tracked(returns(copy))]
    fn c1(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c2(db, seed))
    }

    #[salsa::tracked(returns(copy))]
    fn c2(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c3(db, seed))
    }

    #[salsa::tracked(returns(copy))]
    fn c3(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c4(db, seed))
    }

    #[salsa::tracked(returns(copy))]
    fn c4(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c5(db, seed))
    }

    #[salsa::tracked(returns(copy))]
    fn c5(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c6(db, seed))
    }

    #[salsa::tracked(returns(copy))]
    fn c6(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c7(db, seed))
    }

    #[salsa::tracked(returns(copy))]
    fn c7(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c0(db, seed))
    }

    const _: () = assert!(CYCLE == 8);

    /// The cycle `c0 → c1 → … → c7 → c0` over a seed whose cap each run moves between 3
    /// and 4: from `cycle_initial`'s 0 the first iteration reaches the cap, the second
    /// confirms it.
    pub(crate) struct Cycle {
        db: Db,
        seed: Seed,
        cap: u32,
    }

    impl Cycle {
        pub(crate) fn new() -> Cycle {
            let db = Db::default();
            let seed = Seed::new(&db, 3);
            assert_eq!(c0(&db, seed), 3);
            Cycle { db, seed, cap: 3 }
        }

        pub(crate) fn run(&mut self) -> u32 {
            self.cap ^= 7;
            self.seed.set_cap(&mut self.db).to(self.cap);
            let value = c0(&self.db, self.seed);
            debug_assert_eq!(value, self.cap);
            value
        }
    }
}

fn fork_benchmarks() -> impl IntoBenchmarks {
    [
        benchmark_fn("salsa/hit/sequential", |b| {
            let hits = fork::Hits::new(false);
            let mut at = 0;
            b.iter(move || {
                at = (at + 1) % KEYS;
                fork::double(&hits.db, black_box(hits.order[at]))
            })
        }),
        benchmark_fn("salsa/hit/scattered", |b| {
            let hits = fork::Hits::new(true);
            let mut at = 0;
            b.iter(move || {
                at = (at + 1) % KEYS;
                fork::double(&hits.db, black_box(hits.order[at]))
            })
        }),
        benchmark_fn("salsa/hit_in_query/sequential", |b| {
            let mut sweep = fork::InQuery::new(false);
            b.iter(move || sweep.run())
        }),
        benchmark_fn("salsa/hit_in_query/scattered", |b| {
            let mut sweep = fork::InQuery::new(true);
            b.iter(move || sweep.run())
        }),
        benchmark_fn("salsa/verify/other_durability", |b| {
            let mut verify = fork::Verify::new(false);
            b.iter(move || {
                let sum = verify.run();
                assert_eq!(sum, verify.expected);
                sum
            })
        }),
        benchmark_fn("salsa/verify/same_durability", |b| {
            let mut verify = fork::Verify::new(true);
            b.iter(move || {
                let sum = verify.run();
                assert_eq!(sum, verify.expected);
                sum
            })
        }),
        benchmark_fn("salsa/interned/hit", |b| {
            let hits = fork::Hits::new(true);
            let mut at = 0;
            b.iter(move || {
                at = (at + 1) % KEYS;
                fork::Key::new(&hits.db, black_box(walk(at, true) as u32))
            })
        }),
        benchmark_fn("salsa/cycle/fixpoint", |b| {
            let mut cycle = fork::Cycle::new();
            b.iter(move || cycle.run())
        }),
    ]
}

tango_benchmarks!(fork_benchmarks());
