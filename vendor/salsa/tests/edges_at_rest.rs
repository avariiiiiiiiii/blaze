#![cfg(feature = "inventory")]

//! Settled edges in a real database (`Graph::compact`, src/graph.rs): queries read enough inputs
//! (over 2^20 edges) that new revisions compact the graph, settling memos' edges bit-packed, then
//! settling settled and fresh ones again. Deep verify reads them back: a query whose inputs did
//! not change must be verified through its settled edges without running, one whose input did
//! must run, and every value must match a model.

#![forbid(unsafe_code)]

use std::sync::Mutex;

use salsa::Setter;
use salsa::plumbing::ZalsaDatabase;
use salsa::plumbing::graph::edges_at_rest;

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
struct Query {
    #[returns(ref)]
    cells: Vec<Cell>,
    #[returns(copy)]
    flag: Cell,
    #[returns(copy)]
    flagged: bool,
    #[returns(copy)]
    index: u32,
}

/// The queries that ran, by index.
static RAN: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// The cells' sum, plus the flag's value above bit 32 for a flagged query.
#[salsa::tracked(returns(copy))]
fn sum(db: &dyn salsa::Database, query: Query) -> u64 {
    RAN.lock().unwrap().push(query.index(db));
    let flag = if query.flagged(db) {
        u64::from(query.flag(db).value(db)) << 32
    } else {
        0
    };
    query
        .cells(db)
        .iter()
        .map(|cell| u64::from(cell.value(db)))
        .sum::<u64>()
        + flag
}

const CELLS: u32 = 8192;
/// 136 queries of 8192 reads each are 1.11M edges, past the 2^20 a compaction waits for.
const QUERIES: u32 = 136;

#[test]
fn deep_verify_reads_settled_edges() {
    let mut db = Db::default();
    let cells: Vec<Cell> = (0..CELLS).map(|i| Cell::new(&db, i)).collect();
    let flag = Cell::new(&db, 0);
    let queries: Vec<Query> = (0..QUERIES)
        .map(|i| Query::new(&db, cells.clone(), flag, i % 2 == 1, i))
        .collect();
    let mut values: Vec<u32> = (0..CELLS).collect();
    let mut flag_value = 0u32;

    let check = |db: &Db, values: &[u32], flag_value: u32| {
        let cells: u64 = values.iter().map(|&v| u64::from(v)).sum();
        for (i, &query) in queries.iter().enumerate() {
            let expected = cells + if i % 2 == 1 { u64::from(flag_value) << 32 } else { 0 };
            assert_eq!(sum(db, query), expected, "query {i}");
        }
        let mut ran = std::mem::take(&mut *RAN.lock().unwrap());
        ran.sort_unstable();
        ran
    };

    assert_eq!(check(&db, &values, flag_value), (0..QUERIES).collect::<Vec<_>>());
    let flagged: Vec<u32> = (0..QUERIES).filter(|i| i % 2 == 1).collect();
    let mut compactions = 0;
    for round in 1..=8u32 {
        flag_value = round;
        flag.set_value(&mut db).to(flag_value);
        // A cell every query reads changes in round 6: then every query must run.
        if round == 6 {
            values[7] = 1000;
            cells[7].set_value(&mut db).to(1000);
        }
        let (settled, fresh) = edges_at_rest(db.zalsa());
        if settled > 0 && fresh == 0 {
            compactions += 1;
        }
        let ran = check(&db, &values, flag_value);
        if round == 6 {
            assert_eq!(ran, (0..QUERIES).collect::<Vec<_>>(), "round {round}");
        } else {
            assert_eq!(ran, flagged, "round {round}");
        }
    }
    // Round 1 settles the first revision's edges; a later one settles settled and fresh ones.
    assert!(compactions >= 2, "{compactions} compactions");
}
