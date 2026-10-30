#![cfg(feature = "inventory")]

//! Each run of a tracked function's code is a `blaze_mem::frame::task` of the function's
//! `mem_tag` (`Query` by default): an execution, each fixpoint iteration, its cycle functions;
//! and a run nested in another leaves the outer run's frame as it found it.

use blaze_mem::{Tag, frame};

#[salsa::input]
struct Input {
    #[returns(copy)]
    n: u32,
}

fn tag_now() -> Tag {
    frame::current(|frame| frame.tag())
}

#[salsa::tracked(returns(copy))]
fn plain(db: &dyn salsa::Database, input: Input) -> Tag {
    let _ = input.n(db);
    tag_now()
}

#[salsa::tracked(returns(copy), mem_tag = Infer)]
fn inferred(db: &dyn salsa::Database, input: Input) -> Tag {
    let _ = input.n(db);
    tag_now()
}

#[test]
fn a_run_is_a_task_of_its_functions_tag() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 1);
    assert_eq!(plain(&db, input), Tag::Query);
    assert_eq!(inferred(&db, input), Tag::Infer);
}

const PARSE_TAG: salsa::plumbing::MemTag = salsa::plumbing::MemTag::Mir;

#[salsa::tracked(returns(copy), mem_tag = Parse)]
fn by_name(db: &dyn salsa::Database, input: Input) -> Tag {
    let _ = input.n(db);
    tag_now()
}

#[salsa::tracked(returns(copy), mem_tag = crate::PARSE_TAG)]
fn by_path(db: &dyn salsa::Database, input: Input) -> Tag {
    let _ = input.n(db);
    tag_now()
}

#[test]
fn a_tag_is_a_variant_name_or_a_path_to_a_constant() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 1);
    assert_eq!(by_name(&db, input), Tag::Parse);
    assert_eq!(by_path(&db, input), Tag::Mir);
}

#[salsa::tracked(returns(copy), mem_tag = Mir)]
fn inner(db: &dyn salsa::Database, input: Input) -> Tag {
    let n = u64::from(input.n(db));
    frame::current(|frame| {
        let _ = frame.alloc_slice_copy(&[n; 64]);
        frame.tag()
    })
}

/// The bytes the outer run's frame holds before and after an inner run, the tag the inner run
/// saw, and the outer run's tag after it.
#[salsa::tracked(returns(copy))]
fn outer(db: &dyn salsa::Database, input: Input) -> (usize, usize, Tag, Tag) {
    let before = frame::current(|frame| {
        let _ = frame.alloc_slice_copy(&[0u64; 16]);
        frame.used()
    });
    let inner_tag = inner(db, input);
    let (after, tag) = frame::current(|frame| (frame.used(), frame.tag()));
    (before, after, inner_tag, tag)
}

#[test]
fn a_nested_run_has_a_frame_of_its_own() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 3);
    let (before, after, inner_tag, tag) = outer(&db, input);
    assert!(before >= 128, "the outer run's bytes are in its frame");
    assert_eq!(
        before, after,
        "the inner run's bytes were not left in the outer run's frame"
    );
    assert_eq!(inner_tag, Tag::Mir);
    assert_eq!(tag, Tag::Query);
}

#[salsa::tracked(returns(copy), cycle_initial = initial, cycle_fn = recover, mem_tag = Infer)]
fn fixpoint(db: &dyn salsa::Database, input: Input) -> u32 {
    assert_eq!(tag_now(), Tag::Infer);
    (fixpoint(db, input) + 1).min(input.n(db))
}

fn initial(_db: &dyn salsa::Database, _id: salsa::Id, _input: Input) -> u32 {
    assert_eq!(tag_now(), Tag::Infer);
    0
}

fn recover(
    _db: &dyn salsa::Database,
    _cycle: &salsa::Cycle,
    _last: &u32,
    value: u32,
    _input: Input,
) -> u32 {
    assert_eq!(tag_now(), Tag::Infer);
    value
}

#[test]
fn fixpoint_iterations_and_cycle_functions_run_in_frames() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 3);
    assert_eq!(fixpoint(&db, input), 3);
}

#[salsa::tracked(returns(copy), cycle_result = fallback, mem_tag = Nameres)]
fn immediate(db: &dyn salsa::Database, input: Input) -> u32 {
    assert_eq!(tag_now(), Tag::Nameres);
    immediate(db, input) + input.n(db)
}

fn fallback(_db: &dyn salsa::Database, _id: salsa::Id, _input: Input) -> u32 {
    assert_eq!(tag_now(), Tag::Nameres);
    7
}

#[test]
fn an_immediate_fallback_runs_in_a_frame() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 1);
    assert_eq!(immediate(&db, input), 7);
}
