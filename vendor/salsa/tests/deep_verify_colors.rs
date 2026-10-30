#![cfg(feature = "inventory")]

//! Deep verify through node colors (`graph::colors`): a memo's edges are checked by their
//! dependencies' colors first, and a color may only ever say "unchanged" when the memo would.
//! Debug builds check every color that said so against the memo path; these tests drive the
//! paths that write colors (publish, verify, backdating, input fields, tracked struct deletion,
//! interning, accumulated values, many threads verifying the same memos) and check every result
//! and every re-execution against a model.

#![forbid(unsafe_code)]

use std::sync::Mutex;

use salsa::Setter;

#[salsa::db]
#[derive(Clone, Default)]
struct Db {
    storage: salsa::Storage<Self>,
}

#[salsa::db]
impl salsa::Database for Db {}

#[salsa::input]
struct Cell {
    #[returns(copy)]
    value: u32,
}

#[salsa::input]
struct Grid {
    #[returns(ref)]
    cells: Vec<Cell>,
}

/// The starts of the windows that ran, in `colors_follow_every_change`.
static WINDOWS_RUN: Mutex<Vec<u32>> = Mutex::new(Vec::new());

const WINDOW: u32 = 8;
const STEP: u32 = 4;

/// Backdates when a cell changes but keeps its parity.
#[salsa::tracked(returns(copy))]
fn parity(db: &dyn salsa::Database, cell: Cell) -> u32 {
    cell.value(db) % 2
}

/// Interns `(grid, start)`.
#[salsa::tracked(returns(copy))]
fn window(db: &dyn salsa::Database, grid: Grid, start: u32) -> u32 {
    WINDOWS_RUN.lock().unwrap().push(start);
    let cells = grid.cells(db);
    (start..start + WINDOW)
        .map(|i| parity(db, cells[i as usize]))
        .sum()
}

#[salsa::tracked(returns(copy))]
fn total(db: &dyn salsa::Database, grid: Grid) -> u32 {
    let len = grid.cells(db).len() as u32;
    (0..=(len - WINDOW) / STEP)
        .map(|w| window(db, grid, w * STEP))
        .sum()
}

fn model(values: &[u32]) -> (u32, Vec<u32>) {
    let len = values.len() as u32;
    let windows: Vec<u32> = (0..=(len - WINDOW) / STEP)
        .map(|w| {
            (w * STEP..w * STEP + WINDOW)
                .map(|i| values[i as usize] % 2)
                .sum()
        })
        .collect();
    (windows.iter().sum(), windows)
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

/// Across random edits (a parity flip, a value that keeps its parity, a value set to itself),
/// the total matches the model and a window runs again exactly when one of its cells' parity
/// flipped: `parity` backdates otherwise, so a window is verified by its edges. A color that
/// wrongly said "unchanged" would skip a window that must run; one that was never trusted
/// changes nothing here, but the debug check compares every color that was.
#[test]
fn colors_follow_every_change() {
    let mut db = Db::default();
    let mut values: Vec<u32> = (0..64).collect();
    let cells: Vec<Cell> = values.iter().map(|&v| Cell::new(&db, v)).collect();
    let grid = Grid::new(&db, cells.clone());
    let mut rng = Lcg(7);

    let (expected, _) = model(&values);
    assert_eq!(total(&db, grid), expected);
    WINDOWS_RUN.lock().unwrap().clear();

    for _ in 0..200 {
        let before = values.clone();
        for _ in 0..rng.next() % 4 {
            let i = (rng.next() as usize) % values.len();
            let new = match rng.next() % 3 {
                0 => values[i] + 1,
                1 => values[i] + 2,
                _ => values[i],
            };
            values[i] = new;
            cells[i].set_value(&mut db).to(new);
        }
        let (expected, windows) = model(&values);
        let must_run: Vec<u32> = (0..windows.len() as u32)
            .map(|w| w * STEP)
            .filter(|&start| {
                (start..start + WINDOW).any(|i| before[i as usize] % 2 != values[i as usize] % 2)
            })
            .collect();

        assert_eq!(total(&db, grid), expected);
        let mut ran = std::mem::take(&mut *WINDOWS_RUN.lock().unwrap());
        ran.sort_unstable();
        assert_eq!(ran, must_run);
    }
}

/// Many threads verify and read the same memos after each edit; each sees the model's answer.
#[test]
fn racing_verifiers_agree() {
    let mut db = Db::default();
    let mut values: Vec<u32> = (0..96).map(|v| v * 3).collect();
    let cells: Vec<Cell> = values.iter().map(|&v| Cell::new(&db, v)).collect();
    let grid = Grid::new(&db, cells.clone());
    let mut rng = Lcg(11);

    for _ in 0..30 {
        let i = (rng.next() as usize) % values.len();
        values[i] += 1 + rng.next() % 2;
        cells[i].set_value(&mut db).to(values[i]);
        let (expected, windows) = model(&values);
        std::thread::scope(|scope| {
            for t in 0..8u32 {
                let db = db.clone();
                let windows = &windows;
                scope.spawn(move || {
                    let count = windows.len() as u32;
                    for k in 0..count {
                        let w = (k * 5 + t * 3) % count;
                        assert_eq!(window(&db, grid, w * STEP), windows[w as usize]);
                    }
                    assert_eq!(total(&db, grid), expected);
                });
            }
        });
    }
}

#[salsa::tracked]
struct Odd<'db> {
    #[returns(copy)]
    cell: Cell,
}

/// Tracked structs for the odd cells: an edit that changes which cells are odd deletes some
/// (their memos are taken, so their nodes' colors are forgotten) and makes others.
#[salsa::tracked(returns(ref))]
fn odds(db: &dyn salsa::Database, grid: Grid) -> Vec<Odd<'_>> {
    grid.cells(db)
        .iter()
        .filter(|cell| cell.value(db) % 2 == 1)
        .map(|&cell| Odd::new(db, cell))
        .collect()
}

#[salsa::tracked(returns(copy))]
fn odd_value<'db>(db: &'db dyn salsa::Database, odd: Odd<'db>) -> u32 {
    odd.cell(db).value(db)
}

#[salsa::tracked(returns(copy))]
fn odd_sum(db: &dyn salsa::Database, grid: Grid) -> u32 {
    odds(db, grid).iter().map(|&odd| odd_value(db, odd)).sum()
}

#[test]
fn deleted_structs_forget_their_colors() {
    let mut db = Db::default();
    let mut values: Vec<u32> = (0..40).collect();
    let cells: Vec<Cell> = values.iter().map(|&v| Cell::new(&db, v)).collect();
    let grid = Grid::new(&db, cells.clone());
    let mut rng = Lcg(3);
    for _ in 0..100 {
        let i = (rng.next() as usize) % values.len();
        values[i] += 1 + rng.next() % 2;
        cells[i].set_value(&mut db).to(values[i]);
        let expected: u32 = values.iter().filter(|v| *v % 2 == 1).sum();
        assert_eq!(odd_sum(&db, grid), expected);
    }
}

#[cfg(feature = "accumulator")]
mod accumulated {
    use salsa::{Accumulator, Setter};

    use super::{Cell, Db, Lcg};

    #[salsa::accumulator]
    struct Seen(u32);

    #[salsa::tracked(returns(copy))]
    fn leaf(db: &dyn salsa::Database, cell: Cell) -> u32 {
        let value = cell.value(db);
        if value % 5 == 0 {
            Seen(value).accumulate(db);
        }
        value % 3
    }

    #[salsa::tracked(returns(copy))]
    fn root(db: &dyn salsa::Database, cells: super::Grid) -> u32 {
        cells.cells(db).iter().map(|&cell| leaf(db, cell)).sum()
    }

    /// A memo whose inputs accumulated values is never verified by color alone: after edits
    /// elsewhere, the accumulated values are still found.
    #[test]
    fn accumulated_values_survive_color_verification() {
        let mut db = Db::default();
        let mut values: Vec<u32> = (0..30).collect();
        let cells: Vec<Cell> = values.iter().map(|&v| Cell::new(&db, v)).collect();
        let grid = super::Grid::new(&db, cells.clone());
        let mut rng = Lcg(5);
        for _ in 0..60 {
            let i = (rng.next() as usize) % values.len();
            values[i] += 1 + rng.next() % 4;
            cells[i].set_value(&mut db).to(values[i]);
            assert_eq!(root(&db, grid), values.iter().map(|v| v % 3).sum::<u32>());
            let seen: Vec<u32> = root::accumulated::<Seen>(&db, grid)
                .iter()
                .map(|seen| seen.0)
                .collect();
            let expected: Vec<u32> = values.iter().copied().filter(|v| v % 5 == 0).collect();
            assert_eq!(seen, expected);
        }
    }
}
