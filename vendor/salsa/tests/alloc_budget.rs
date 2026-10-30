#![cfg(feature = "inventory")]

//! Allocation budgets for salsa's hot paths (docs/v2/plan/goals/principles.md): a memo hit, an
//! interner hit and a field read allocate nothing. Each test warms the path first, so one-time
//! setup (the thread's task state, a frame's first edge buffer) is outside the measured closure.

#![forbid(unsafe_code)]

use blaze_alloc_count::{Counting, assert_at_most, measure};
use salsa::Setter;

#[global_allocator]
static ALLOC: Counting = Counting;

#[salsa::input]
struct Input {
    #[returns(copy)]
    n: u32,
}

#[salsa::interned]
struct Word<'db> {
    n: u32,
}

#[salsa::tracked]
struct Pair<'db> {
    #[returns(copy)]
    n: u32,
}

#[salsa::tracked(returns(copy))]
fn double(db: &dyn salsa::Database, input: Input) -> u32 {
    input.n(db) * 2
}

#[salsa::tracked(returns(copy), lru = 4)]
fn triple(db: &dyn salsa::Database, input: Input) -> u32 {
    input.n(db) * 3
}

#[salsa::tracked(returns(copy))]
fn add(db: &dyn salsa::Database, input: Input, k: u32) -> u32 {
    input.n(db) + k
}

#[salsa::tracked(returns(copy))]
fn pair(db: &dyn salsa::Database, input: Input) -> Pair<'_> {
    Pair::new(db, input.n(db))
}

/// Reads `a`'s memo (so the frame's edge buffer exists), then counts what a hit on `b`'s memo
/// allocates inside this query, recording its read included.
#[salsa::tracked(returns(copy))]
fn hit_inside_a_query(db: &dyn salsa::Database, a: Input, b: Input) -> u64 {
    double(db, a);
    let (_, used) = measure(|| double(db, b));
    used.allocations
}

#[test]
fn a_memo_hit_allocates_nothing() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 21);
    assert_eq!(double(&db, input), 42);
    let value = assert_at_most(0, "a memo hit", || double(&db, input));
    assert_eq!(value, 42);
}

#[test]
fn a_memo_hit_inside_a_query_allocates_nothing() {
    let db = salsa::DatabaseImpl::new();
    let a = Input::new(&db, 1);
    let b = Input::new(&db, 2);
    double(&db, b);
    assert_eq!(hit_inside_a_query(&db, a, b), 0, "a memo hit inside a query");
}

#[test]
fn an_lru_memo_hit_allocates_nothing() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 5);
    triple(&db, input);
    let value = assert_at_most(0, "a hit on an `lru` function", || triple(&db, input));
    assert_eq!(value, 15);
}

#[test]
fn a_multi_argument_memo_hit_allocates_nothing() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 5);
    add(&db, input, 7);
    let value = assert_at_most(0, "a hit on a multi-argument function", || add(&db, input, 7));
    assert_eq!(value, 12);
}

#[test]
fn an_interner_hit_allocates_nothing() {
    let db = salsa::DatabaseImpl::new();
    let first = Word::new(&db, 7);
    let again = assert_at_most(0, "an interner hit", || Word::new(&db, 7));
    assert_eq!(first, again);
}

#[test]
fn field_reads_allocate_nothing() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 9);
    let tracked = pair(&db, input);
    input.n(&db);
    tracked.n(&db);
    assert_eq!(assert_at_most(0, "an input field read", || input.n(&db)), 9);
    assert_eq!(assert_at_most(0, "a tracked struct field read", || tracked.n(&db)), 9);
}

#[salsa::tracked(returns(copy))]
fn link0(db: &dyn salsa::Database, input: Input) -> u32 {
    input.n(db)
}

#[salsa::tracked(returns(copy))]
fn link1(db: &dyn salsa::Database, input: Input) -> u32 {
    link0(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link2(db: &dyn salsa::Database, input: Input) -> u32 {
    link1(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link3(db: &dyn salsa::Database, input: Input) -> u32 {
    link2(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link4(db: &dyn salsa::Database, input: Input) -> u32 {
    link3(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link5(db: &dyn salsa::Database, input: Input) -> u32 {
    link4(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link6(db: &dyn salsa::Database, input: Input) -> u32 {
    link5(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link7(db: &dyn salsa::Database, input: Input) -> u32 {
    link6(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link8(db: &dyn salsa::Database, input: Input) -> u32 {
    link7(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link9(db: &dyn salsa::Database, input: Input) -> u32 {
    link8(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link10(db: &dyn salsa::Database, input: Input) -> u32 {
    link9(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link11(db: &dyn salsa::Database, input: Input) -> u32 {
    link10(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn parity(db: &dyn salsa::Database, input: Input) -> u32 {
    input.n(db) % 2
}

#[salsa::tracked(returns(copy))]
fn above_parity(db: &dyn salsa::Database, input: Input) -> u32 {
    parity(db, input) + 1
}

/// A chain of twelve memos, deeper than the eight the walk's stack held inline, verified
/// unchanged after an edit elsewhere: the claim-free walk reuses the thread's stack.
#[test]
fn a_deep_verify_that_changes_nothing_allocates_nothing() {
    let mut db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 1);
    let other = Input::new(&db, 0);
    assert_eq!(link11(&db, input), 12);
    other.set_n(&mut db).to(1);
    assert_eq!(link11(&db, input), 12);
    other.set_n(&mut db).to(2);
    let value = assert_at_most(0, "a deep verify that changes nothing", || link11(&db, input));
    assert_eq!(value, 12);
}

/// The warm steady state of a re-check: an edit that `parity` absorbs runs it again (a frame
/// task, its edges into the edge array, its memo into its column's slab) and backdates it, so
/// `above_parity` is only verified. Three rounds warm the slab's free list and the frame.
#[test]
fn a_warm_recheck_allocates_nothing() {
    let mut db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 0);
    assert_eq!(above_parity(&db, input), 1);
    for n in [2, 4, 6] {
        input.set_n(&mut db).to(n);
        assert_eq!(above_parity(&db, input), 1);
    }
    input.set_n(&mut db).to(8);
    let value =
        assert_at_most(0, "the warm steady state of a re-check", || above_parity(&db, input));
    assert_eq!(value, 1);
}
