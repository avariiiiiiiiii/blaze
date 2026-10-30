#![cfg(feature = "inventory")]

//! Input-closure filters (`graph::bloom`) are a proof, not a guess: a memo they call green
//! is one the deep verify walk would also have kept. Two databases take the same random
//! edits, one with the filters and one without; after every edit round every query asked
//! returns the same value in both, the same functions run in the same order in both, and
//! both agree with a database built fresh from the final inputs. The graph mixes what the
//! filter must handle: input fields at every durability, dependencies that change with the
//! values read, a tracked struct whose field is read by a query keyed on it, and interned
//! values (whose reads make a memo opaque). More than `bloom::WINDOW` revisions pass, so
//! memos verified outside the change log's window take the walk again.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use blaze_ids::rng::SplitMix64;
use salsa::plumbing::ZalsaDatabase as _;
use salsa::{Durability, Setter};

#[salsa::db]
trait Db: salsa::Database {
    fn log(&self, line: String);
}

#[salsa::db]
struct LogDb {
    storage: salsa::Storage<Self>,
    log: Arc<Mutex<Vec<String>>>,
    validated: Arc<AtomicUsize>,
}

impl LogDb {
    fn new(filters: bool) -> Self {
        let validated = Arc::new(AtomicUsize::new(0));
        let storage = salsa::Storage::new(Some(Box::new({
            let validated = Arc::clone(&validated);
            move |event| {
                if let salsa::EventKind::DidValidateMemoizedValue { .. } = event.kind {
                    validated.fetch_add(1, Ordering::Relaxed);
                }
            }
        })));
        let mut db = Self {
            storage,
            log: Arc::default(),
            validated,
        };
        db.zalsa_mut().set_closure_filters(filters);
        db
    }

    fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }
}

#[salsa::db]
impl salsa::Database for LogDb {}

#[salsa::db]
impl Db for LogDb {
    fn log(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }
}

#[salsa::input]
struct Cell {
    #[returns(copy)]
    value: u32,
}

#[salsa::input]
struct Spec {
    #[returns(copy)]
    index: u32,
    #[returns(ref)]
    reads: Vec<Cell>,
    #[returns(ref)]
    deps: Vec<Spec>,
    #[returns(copy)]
    gate: u32,
}

#[salsa::tracked]
struct Made<'db> {
    #[tracked]
    #[returns(copy)]
    sum: u64,
}

#[salsa::interned]
struct Tag<'db> {
    #[returns(copy)]
    n: u32,
}

fn mix(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29)
}

/// Reads its cells and its dependencies, stopping early on some values so the edges it
/// records change with what it read.
#[salsa::tracked(returns(copy))]
fn eval(db: &dyn Db, spec: Spec) -> u64 {
    let index = spec.index(db);
    db.log(format!("eval {index}"));
    let mut h = mix(0, u64::from(index));
    for (i, cell) in spec.reads(db).iter().enumerate() {
        let value = cell.value(db);
        h = mix(h, u64::from(value));
        if i == 0 && value % 5 == 0 {
            return h;
        }
    }
    let gate = spec.gate(db);
    for dep in spec.deps(db) {
        let d = eval(db, *dep);
        h = mix(h, d % 7);
        if d % 4 == u64::from(gate % 4) {
            break;
        }
    }
    match gate % 3 {
        1 => mix(h, made_sum(db, spec)),
        2 => mix(h, u64::from(Tag::new(db, (h % 3) as u32).n(db))),
        _ => h,
    }
}

/// Creates a tracked struct; `read_made` reads its field through a query keyed on it.
#[salsa::tracked]
fn make(db: &dyn Db, spec: Spec) -> Made<'_> {
    db.log(format!("make {}", spec.index(db)));
    let sum = spec.reads(db).iter().map(|cell| u64::from(cell.value(db))).sum();
    Made::new(db, sum)
}

#[salsa::tracked(returns(copy))]
fn read_made<'db>(db: &'db dyn Db, made: Made<'db>) -> u64 {
    let sum = made.sum(db);
    db.log(format!("read_made {sum}"));
    sum % 11
}

#[salsa::tracked(returns(copy))]
fn made_sum(db: &dyn Db, spec: Spec) -> u64 {
    read_made(db, make(db, spec))
}

struct SpecModel {
    reads: Vec<usize>,
    deps: Vec<usize>,
    gate: u32,
}

/// The inputs both databases hold, to build a fresh one from.
struct World {
    cells: Vec<(u32, Durability)>,
    specs: Vec<SpecModel>,
}

fn durability(rng: &mut SplitMix64) -> Durability {
    match rng.below(10) {
        0 => Durability::HIGH,
        1 | 2 => Durability::MEDIUM,
        _ => Durability::LOW,
    }
}

fn picks(rng: &mut SplitMix64, below: usize, most: u64) -> Vec<usize> {
    if below == 0 {
        return Vec::new();
    }
    (0..rng.below(most + 1)).map(|_| rng.index(below)).collect()
}

impl World {
    fn random(rng: &mut SplitMix64, cells: usize, specs: usize) -> World {
        let cells: Vec<(u32, Durability)> = (0..cells)
            .map(|_| (rng.below(12) as u32, durability(rng)))
            .collect();
        let cell_count = cells.len();
        let specs = (0..specs)
            .map(|i| SpecModel {
                reads: picks(rng, cell_count, 4),
                deps: picks(rng, i, 3),
                gate: rng.next_u32(),
            })
            .collect();
        World { cells, specs }
    }

    fn build(&self, db: &LogDb) -> (Vec<Cell>, Vec<Spec>) {
        let cells: Vec<Cell> = self
            .cells
            .iter()
            .map(|&(value, d)| Cell::builder(value).value_durability(d).new(db))
            .collect();
        let mut specs: Vec<Spec> = Vec::with_capacity(self.specs.len());
        for (i, spec) in self.specs.iter().enumerate() {
            let reads = spec.reads.iter().map(|&c| cells[c]).collect();
            let deps = spec.deps.iter().map(|&s| specs[s]).collect();
            specs.push(Spec::new(db, i as u32, reads, deps, spec.gate));
        }
        (cells, specs)
    }
}

enum Edit {
    Cell { cell: usize, value: u32, durability: Durability },
    Deps { spec: usize, deps: Vec<usize> },
}

fn run(seed: u64) -> (usize, usize) {
    let mut rng = SplitMix64::new(seed);
    let mut world = World::random(&mut rng, 60, 40);
    let mut on = LogDb::new(true);
    let mut off = LogDb::new(false);
    let (on_cells, on_specs) = world.build(&on);
    let (off_cells, off_specs) = world.build(&off);
    let top = world.specs.len() - 1;
    for round in 0..120 {
        for _ in 0..=rng.below(3) {
            let edit = if rng.one_in(20) {
                let spec = rng.index(world.specs.len());
                Edit::Deps {
                    spec,
                    deps: picks(&mut rng, spec, 3),
                }
            } else {
                Edit::Cell {
                    cell: rng.index(world.cells.len()),
                    value: rng.below(12) as u32,
                    durability: durability(&mut rng),
                }
            };
            match edit {
                Edit::Cell {
                    cell,
                    value,
                    durability,
                } => {
                    world.cells[cell] = (value, durability);
                    on_cells[cell]
                        .set_value(&mut on)
                        .with_durability(durability)
                        .to(value);
                    off_cells[cell]
                        .set_value(&mut off)
                        .with_durability(durability)
                        .to(value);
                }
                Edit::Deps { spec, deps } => {
                    let on_deps = deps.iter().map(|&s| on_specs[s]).collect();
                    let off_deps = deps.iter().map(|&s| off_specs[s]).collect();
                    world.specs[spec].deps = deps;
                    on_specs[spec].set_deps(&mut on).to(on_deps);
                    off_specs[spec].set_deps(&mut off).to(off_deps);
                }
            }
        }
        let asked: Vec<usize> = (0..world.specs.len())
            .filter(|&s| s == top || rng.one_in(3))
            .collect();
        let with: Vec<u64> = asked.iter().map(|&s| eval(&on, on_specs[s])).collect();
        let without: Vec<u64> = asked.iter().map(|&s| eval(&off, off_specs[s])).collect();
        assert_eq!(with, without, "seed {seed} round {round}: results differ");
        assert_eq!(
            on.take_log(),
            off.take_log(),
            "seed {seed} round {round}: different functions ran"
        );
        if round % 40 == 39 {
            let fresh = LogDb::new(true);
            let (_, fresh_specs) = world.build(&fresh);
            let scratch: Vec<u64> = asked.iter().map(|&s| eval(&fresh, fresh_specs[s])).collect();
            assert_eq!(with, scratch, "seed {seed} round {round}: stale result");
        }
    }
    (
        on.validated.load(Ordering::Relaxed),
        off.validated.load(Ordering::Relaxed),
    )
}

#[test]
fn filters_change_no_result_and_no_execution() {
    let mut proved = 0;
    for seed in 0..6 {
        let (with, without) = run(seed);
        assert!(with <= without, "seed {seed}: the filter validated more ({with} > {without})");
        proved += without - with;
    }
    assert!(proved > 0, "the filter never kept a memo green without a walk");
}
