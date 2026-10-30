#![cfg(feature = "inventory")]

//! The live graph renumbered in walk order at a compaction (`Graph::compact_renumbered`,
//! src/graph.rs; docs/v2/plan/lanes/db/graph.md, section 2). With every revision compacting,
//! the same edits run the same queries again, with the same values, whether the renumbering is
//! on or off: through backdating, durabilities, tracked structs (whose creator the filter column
//! names by node), a fixpoint cycle and accumulated values. And a task whose node cache was
//! filled before a renumbering records the right edges after it: its stale cache is dropped.

#![forbid(unsafe_code)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use salsa::plumbing::ZalsaDatabase;
use salsa::plumbing::graph::edges_at_rest;
use salsa::{Durability, Setter};

#[salsa::db]
#[derive(Default)]
struct Db {
    storage: salsa::Storage<Self>,
    ran: Mutex<Vec<(Ran, u32)>>,
    /// The task the current thread runs: each depth has its own query stack and node cache.
    depth: AtomicU32,
}

#[salsa::db]
impl salsa::Database for Db {
    fn task_depth(&self) -> u32 {
        self.depth.load(Ordering::Relaxed)
    }
}

/// Which query ran.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Ran {
    Window,
    Total,
    Odds,
    OddValue,
    Climb,
    Leaf,
}

impl Db {
    fn ran(&self, what: Ran, id: u32) {
        self.ran.lock().unwrap().push((what, id));
    }

    fn take_ran(&self) -> Vec<(Ran, u32)> {
        let mut ran = std::mem::take(&mut *self.ran.lock().unwrap());
        ran.sort_unstable();
        ran
    }

    fn run_as(&self, depth: u32) {
        self.depth.store(depth, Ordering::Relaxed);
    }
}

#[salsa::input]
struct Cell {
    #[returns(copy)]
    value: u32,
    #[returns(copy)]
    index: u32,
}

#[salsa::input]
struct Grid {
    #[returns(ref)]
    cells: Vec<Cell>,
}

const CELLS: u32 = 96;
const WINDOW: u32 = 8;
const STEP: u32 = 4;
/// The cells `climb` runs on.
const CLIMBERS: u32 = 12;

/// Backdates when a cell changes but keeps its parity.
#[salsa::tracked(returns(copy))]
fn parity(db: &Db, cell: Cell) -> u32 {
    cell.value(db) % 2
}

#[salsa::tracked(returns(copy))]
fn window(db: &Db, grid: Grid, start: u32) -> u32 {
    db.ran(Ran::Window, start);
    let cells = grid.cells(db);
    (start..start + WINDOW)
        .map(|i| parity(db, cells[i as usize]))
        .sum()
}

#[salsa::tracked(returns(copy))]
fn total(db: &Db, grid: Grid) -> u32 {
    db.ran(Ran::Total, 0);
    (0..=(CELLS - WINDOW) / STEP)
        .map(|w| window(db, grid, w * STEP))
        .sum()
}

#[salsa::tracked]
struct Odd<'db> {
    #[returns(copy)]
    cell: Cell,
}

#[salsa::tracked(returns(ref))]
fn odds(db: &Db, grid: Grid) -> Vec<Odd<'_>> {
    db.ran(Ran::Odds, 0);
    grid.cells(db)
        .iter()
        .filter(|cell| cell.value(db) % 2 == 1)
        .map(|&cell| Odd::new(db, cell))
        .collect()
}

#[salsa::tracked(returns(copy))]
fn odd_value<'db>(db: &'db Db, odd: Odd<'db>) -> u32 {
    let cell = odd.cell(db);
    db.ran(Ran::OddValue, cell.index(db));
    cell.value(db)
}

#[salsa::tracked(returns(copy))]
fn odd_sum(db: &Db, grid: Grid) -> u32 {
    odds(db, grid).iter().map(|&odd| odd_value(db, odd)).sum()
}

/// A fixpoint of `climb` and `step`: one more an iteration, up to the cell's value mod 7.
#[salsa::tracked(returns(copy), cycle_initial = climb_initial)]
fn climb(db: &Db, cell: Cell) -> u32 {
    db.ran(Ran::Climb, cell.index(db));
    step(db, cell).min(cell.value(db) % 7)
}

#[salsa::tracked(returns(copy))]
fn step(db: &Db, cell: Cell) -> u32 {
    climb(db, cell) + 1
}

fn climb_initial(_db: &Db, _id: salsa::Id, _cell: Cell) -> u32 {
    0
}

#[cfg(feature = "accumulator")]
#[salsa::accumulator]
struct Seen(u32);

#[salsa::tracked(returns(copy))]
fn leaf(db: &Db, cell: Cell) -> u32 {
    db.ran(Ran::Leaf, cell.index(db));
    let value = cell.value(db);
    #[cfg(feature = "accumulator")]
    {
        use salsa::Accumulator;
        if value % 5 == 0 {
            Seen(value).accumulate(db);
        }
    }
    value % 3
}

#[salsa::tracked(returns(copy))]
fn leaves(db: &Db, grid: Grid) -> u32 {
    grid.cells(db).iter().map(|&cell| leaf(db, cell)).sum()
}

/// A small deterministic generator, so a failure replays.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }
}

fn windows(values: &[u32]) -> Vec<u32> {
    (0..=(CELLS - WINDOW) / STEP)
        .map(|w| {
            (w * STEP..w * STEP + WINDOW)
                .map(|i| values[i as usize] % 2)
                .sum()
        })
        .collect()
}

/// Every query's value, checked against the model.
fn values_of(db: &Db, grid: Grid, cells: &[Cell], values: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    let windows = windows(values);
    out.push(total(db, grid));
    assert_eq!(out[0], windows.iter().sum::<u32>(), "total");
    out.push(odd_sum(db, grid));
    assert_eq!(out[1], values.iter().filter(|v| *v % 2 == 1).sum::<u32>(), "odd_sum");
    out.push(leaves(db, grid));
    assert_eq!(out[2], values.iter().map(|v| v % 3).sum::<u32>(), "leaves");
    for i in 0..CLIMBERS as usize {
        let value = climb(db, cells[i]);
        assert_eq!(value, values[i] % 7, "climb {i}");
        out.push(value);
    }
    #[cfg(feature = "accumulator")]
    {
        let seen: Vec<u32> = leaves::accumulated::<Seen>(db, grid)
            .iter()
            .map(|seen| seen.0)
            .collect();
        let expected: Vec<u32> = values.iter().copied().filter(|v| v % 5 == 0).collect();
        assert_eq!(seen, expected, "accumulated");
        out.extend(seen);
    }
    out
}

/// One run of seeded edits over a database that compacts at every revision, renumbering in
/// walk order or not: per round, what ran again and every value.
fn run(walk_order: bool) -> Vec<(Vec<(Ran, u32)>, Vec<u32>)> {
    let mut db = Db::default();
    db.zalsa_mut().compact_every_revision(true);
    db.zalsa_mut().set_walk_order(walk_order);
    let mut values: Vec<u32> = (0..CELLS).map(|v| v * 3).collect();
    // Every eighth cell is of high durability.
    let durability = |i: usize| if i % 8 == 0 { Durability::HIGH } else { Durability::LOW };
    let cells: Vec<Cell> = (0..CELLS as usize)
        .map(|i| {
            Cell::builder(values[i], i as u32)
                .value_durability(durability(i))
                .new(&db)
        })
        .collect();
    let grid = Grid::new(&db, cells.clone());
    let mut rng = Lcg(0x2545_f491);
    let mut rounds = vec![(Vec::new(), values_of(&db, grid, &cells, &values))];
    db.take_ran();
    let mut compactions = 0;
    for _ in 0..80 {
        for _ in 0..rng.next() % 4 {
            let i = (rng.next() % CELLS) as usize;
            values[i] = match rng.next() % 3 {
                0 => values[i] + 1,
                1 => values[i] + 2,
                _ => values[i],
            };
            cells[i]
                .set_value(&mut db)
                .with_durability(durability(i))
                .to(values[i]);
        }
        let (settled, fresh) = edges_at_rest(db.zalsa());
        if settled > 0 && fresh == 0 {
            compactions += 1;
        }
        let now = values_of(&db, grid, &cells, &values);
        rounds.push((db.take_ran(), now));
    }
    assert!(compactions > 40, "{compactions} compactions");
    rounds
}

#[test]
fn renumbering_runs_the_same_queries_again() {
    let renumbered = run(true);
    let as_found = run(false);
    assert_eq!(renumbered.len(), as_found.len());
    for (round, (with, without)) in renumbered.iter().zip(&as_found).enumerate() {
        assert_eq!(with, without, "round {round}");
    }
}

/// A second task (depth 1 on this thread) caches the cells' nodes, and runs again only after
/// renumberings: the windows it runs record their reads through that cache. Were a stale node
/// kept, a window's edges would name other keys, and the next edit to one of its cells would
/// leave it unrun (or run another).
#[test]
fn a_task_cache_from_before_a_renumbering_is_dropped() {
    let mut db = Db::default();
    db.zalsa_mut().compact_every_revision(true);
    let mut values: Vec<u32> = (0..CELLS).collect();
    let cells: Vec<Cell> = (0..CELLS).map(|i| Cell::new(&db, values[i as usize], i)).collect();
    let grid = Grid::new(&db, cells.clone());
    db.run_as(1);
    assert_eq!(total(&db, grid), windows(&values).iter().sum::<u32>());
    db.run_as(0);
    db.take_ran();
    let mut rng = Lcg(17);
    for round in 0..60 {
        let i = (rng.next() % CELLS) as usize;
        values[i] += 1;
        cells[i].set_value(&mut db).to(values[i]);
        let mut must_run: Vec<(Ran, u32)> = (0..=(CELLS - WINDOW) / STEP)
            .map(|w| w * STEP)
            .filter(|&start| (start..start + WINDOW).contains(&(i as u32)))
            .map(|start| (Ran::Window, start))
            .collect();
        must_run.push((Ran::Total, 0));
        must_run.sort_unstable();
        // Every other round, the task of the stale cache runs the invalidated windows.
        db.run_as(round % 2);
        assert_eq!(total(&db, grid), windows(&values).iter().sum::<u32>(), "round {round}");
        db.run_as(0);
        assert_eq!(db.take_ran(), must_run, "round {round}");
    }
}
