//! Input-closure filters (`graph::bloom`): re-verifying after an edit that touches one module of
//! many, with the filters on and off, and how much a 64-bit filter proves against a 128-bit one.
//!
//! The shape: `MODULES` chains of `DEPTH` memos (`fold` over a `Module` and the one below it),
//! each over its own cells, and `total` over every chain's head. One edit writes a cell of
//! chain 0's head, then `total` is asked again. Without filters every chain is walked to its
//! bottom: `MODULES × DEPTH` memos verified, ≈ 50 ns each from cache, ≈ 5 ms. With them chain
//! 0's head re-executes and every other head is green by one filter check (the head's column
//! word and the change log's word: two loads and an AND, inside the ≈ 20 ns of finding the memo
//! at all), and no chain is walked.
//!
//! Bound (docs/v2/plan/lanes/db/graph.md, "Input-closure filters"): `MODULES` memo lookups and
//! filter checks plus chain 0's head re-executing, ≈ 1000 × 20 ns + 1 µs ≈ 21 µs a round.
//! Target: `reverify` with filters within 2x of that (≤ 45 µs), and at least 100x under the
//! same bench without them.
//!
//! `main` also prints, for a random graph of 20k queries over 2k inputs, the fraction of
//! queries an edit truly leaves alone and the fraction a 64-bit and a 128-bit filter prove so
//! (`graph_bench::bloom::green_rates`): the case for 64 bits is that 128 proves little more.

#![forbid(unsafe_code)]

use std::hint::black_box;

use salsa::plumbing::ZalsaDatabase as _;
use salsa::Setter;

const MODULES: usize = 1000;
const DEPTH: usize = 100;
const CELLS: usize = 4;

fn main() {
    for (inputs, nodes) in [(2_000, 20_000), (20_000, 20_000)] {
        let (truly, proved_64) = salsa::graph_bench::bloom::green_rates::<1>(3, inputs, nodes, 200);
        let (_, proved_128) = salsa::graph_bench::bloom::green_rates::<2>(3, inputs, nodes, 200);
        eprintln!(
            "graph_bloom: {nodes} queries over {inputs} inputs: unaffected {truly:.3}, proved by \
             64 bits {proved_64:.3}, by 128 bits {proved_128:.3}"
        );
    }
    divan::main();
}

#[salsa::input]
struct Cell {
    #[returns(copy)]
    value: u32,
}

#[salsa::input]
struct Module {
    #[returns(ref)]
    cells: Vec<Cell>,
    #[returns(copy)]
    below: Option<Module>,
}

#[salsa::input]
struct Program {
    #[returns(ref)]
    heads: Vec<Module>,
}

#[salsa::tracked(returns(copy))]
#[inline(never)]
fn fold(db: &dyn salsa::Database, module: Module) -> u64 {
    let own: u64 = module.cells(db).iter().map(|cell| u64::from(cell.value(db))).sum();
    own + module.below(db).map_or(0, |below| fold(db, below))
}

#[salsa::tracked(returns(copy))]
#[inline(never)]
fn total(db: &dyn salsa::Database, program: Program) -> u64 {
    program.heads(db).iter().map(|&head| fold(db, head)).sum()
}

#[divan::bench(args = [true, false])]
fn reverify(bencher: divan::Bencher, filters: bool) {
    let mut db = salsa::DatabaseImpl::default();
    db.zalsa_mut().set_closure_filters(filters);
    let mut edited = None;
    let heads: Vec<Module> = (0..MODULES)
        .map(|m| {
            let mut below = None;
            for d in 0..DEPTH {
                let cells: Vec<Cell> = (0..CELLS)
                    .map(|c| Cell::new(&db, (m * 31 + d * 7 + c) as u32 % 13))
                    .collect();
                if m == 0 && d == DEPTH - 1 {
                    edited = Some(cells[0]);
                }
                below = Some(Module::new(&db, cells, below));
            }
            below.expect("DEPTH > 0")
        })
        .collect();
    let edited = edited.expect("MODULES > 0");
    let program = Program::new(&db, heads);
    let base = total(&db, program);
    let start = u64::from(edited.value(&db));
    let mut round = 0u32;
    bencher.bench_local(|| {
        round = round.wrapping_add(1);
        let value = round % 2;
        edited.set_value(&mut db).to(value);
        let sum = total(&db, program);
        assert_eq!(sum, base - start + u64::from(value));
        black_box(sum)
    });
}
