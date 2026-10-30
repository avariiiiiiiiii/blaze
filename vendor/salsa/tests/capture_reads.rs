#![cfg(feature = "inventory")]

//! `capture_reads`, `replay_reads` and `unshareable_reads` (v2/requests/db-ty-solver.md section
//! 4, v2/requests/db-ty.md section 13): what a capture read is read by the query running it, and
//! a query that replays it reruns exactly as if it had made those reads.
//!
//! [`shared`] shares a result across queries as hir-ty's solver cache shares a solved goal: the
//! first query to ask for it in a revision computes it in a capture, and every later one replays
//! the capture.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use salsa::{Accumulator, Durability, Reads, Revision, Setter};

#[salsa::db]
trait Db: salsa::Database {
    fn log(&self, entry: String);
    fn cached(&self, key: &'static str, revision: Revision) -> Option<(u32, Reads)>;
    fn cache(&self, key: &'static str, revision: Revision, value: u32, reads: Reads);
    /// The reads cached for `key` in any revision.
    fn cached_in_any_revision(&self, key: &'static str) -> Option<Reads>;
    fn verdict(&self, what: &'static str, shareable: bool);
}

#[salsa::db]
#[derive(Default)]
struct CaptureDb {
    storage: salsa::Storage<Self>,
    log: Mutex<Vec<String>>,
    shared: Mutex<HashMap<(&'static str, Revision), (u32, Reads)>>,
    verdicts: Mutex<Vec<(&'static str, bool)>>,
}

impl CaptureDb {
    fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }

    fn take_verdicts(&self) -> Vec<(&'static str, bool)> {
        std::mem::take(&mut *self.verdicts.lock().unwrap())
    }
}

#[salsa::db]
impl salsa::Database for CaptureDb {}

#[salsa::db]
impl Db for CaptureDb {
    fn log(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }

    fn cached(&self, key: &'static str, revision: Revision) -> Option<(u32, Reads)> {
        self.shared.lock().unwrap().get(&(key, revision)).copied()
    }

    fn cache(&self, key: &'static str, revision: Revision, value: u32, reads: Reads) {
        self.shared
            .lock()
            .unwrap()
            .insert((key, revision), (value, reads));
    }

    fn cached_in_any_revision(&self, key: &'static str) -> Option<Reads> {
        self.shared
            .lock()
            .unwrap()
            .iter()
            .find(|((cached, _), _)| *cached == key)
            .map(|(_, &(_, reads))| reads)
    }

    fn verdict(&self, what: &'static str, shareable: bool) {
        self.verdicts.lock().unwrap().push((what, shareable));
    }
}

/// `compute`'s result, computed in a capture by the first query to ask for `key` in this
/// revision and replayed into every later one.
fn shared(db: &dyn Db, key: &'static str, compute: impl FnOnce() -> u32) -> u32 {
    let revision = salsa::plumbing::current_revision(db);
    if let Some((value, reads)) = db.cached(key, revision) {
        salsa::replay_reads(db, reads);
        return value;
    }
    db.log(format!("goal {key}"));
    let (value, reads) = salsa::capture_reads(db, compute);
    let reads = reads.unwrap_or_else(|| panic!("goal {key} may not be shared"));
    db.cache(key, revision, value, reads);
    value
}

#[salsa::input]
struct Number {
    #[returns(copy)]
    value: u32,
}

#[salsa::tracked(returns(copy))]
fn outer(db: &dyn Db, x: Number) -> u32 {
    db.log("outer".to_string());
    shared(db, "x", || x.value(db) * 10) + 1
}

#[salsa::tracked(returns(copy))]
fn other(db: &dyn Db, x: Number) -> u32 {
    db.log("other".to_string());
    shared(db, "x", || x.value(db) * 10) + 2
}

#[test]
fn a_capture_merges_into_the_query_running_it() {
    let mut db = CaptureDb::default();
    let x = Number::new(&db, 1);
    let unrelated = Number::new(&db, 0);

    assert_eq!(outer(&db, x), 11);
    assert_eq!(db.take_log(), ["outer", "goal x"]);

    // `outer` read `x` only in the capture.
    x.set_value(&mut db).to(2);
    assert_eq!(outer(&db, x), 21);
    assert_eq!(db.take_log(), ["outer", "goal x"]);

    unrelated.set_value(&mut db).to(5);
    assert_eq!(outer(&db, x), 21);
    assert!(db.take_log().is_empty());
}

#[test]
fn replayed_reads_rerun_the_query_that_replayed_them() {
    let mut db = CaptureDb::default();
    let x = Number::new(&db, 1);
    let unrelated = Number::new(&db, 0);

    assert_eq!(outer(&db, x), 11);
    assert_eq!(other(&db, x), 12);
    assert_eq!(db.take_log(), ["outer", "goal x", "other"]);

    // `other` read nothing itself: only the replayed edge to `x` can rerun it.
    x.set_value(&mut db).to(2);
    assert_eq!(other(&db, x), 22);
    assert_eq!(db.take_log(), ["other", "goal x"]);
    assert_eq!(outer(&db, x), 21);
    assert_eq!(db.take_log(), ["outer"]);

    unrelated.set_value(&mut db).to(5);
    assert_eq!(other(&db, x), 22);
    assert_eq!(outer(&db, x), 21);
    assert!(db.take_log().is_empty());
}

#[salsa::tracked(returns(copy))]
fn steady(db: &dyn Db, high: Number, x: Number) -> u32 {
    db.log("steady".to_string());
    high.value(db) + shared(db, "x", || x.value(db) * 10)
}

#[test]
fn replayed_reads_lower_the_durability_of_the_reader() {
    let mut db = CaptureDb::default();
    let high = Number::builder(100)
        .value_durability(Durability::HIGH)
        .new(&db);
    let x = Number::new(&db, 1);

    assert_eq!(outer(&db, x), 11);
    assert_eq!(steady(&db, high, x), 110);
    assert_eq!(db.take_log(), ["outer", "goal x", "steady"]);

    // Had `steady` kept HIGH durability, a LOW edit would not even verify its edges.
    x.set_value(&mut db).to(2);
    assert_eq!(steady(&db, high, x), 120);
    assert_eq!(db.take_log(), ["steady", "goal x"]);
}

#[salsa::tracked(returns(copy))]
fn parity(db: &dyn Db, x: Number) -> u32 {
    db.log(format!("parity({})", x.value(db)));
    x.value(db) % 2
}

#[salsa::tracked(returns(copy))]
fn parity_outer(db: &dyn Db, x: Number) -> u32 {
    db.log("parity_outer".to_string());
    shared(db, "parity", || parity(db, x))
}

#[salsa::tracked(returns(copy))]
fn parity_other(db: &dyn Db, x: Number) -> u32 {
    db.log("parity_other".to_string());
    shared(db, "parity", || parity(db, x)) + 1
}

#[test]
fn an_equal_value_behind_a_replayed_edge_does_not_rerun_the_reader() {
    let mut db = CaptureDb::default();
    let x = Number::new(&db, 1);

    assert_eq!(parity_outer(&db, x), 1);
    assert_eq!(parity_other(&db, x), 2);
    assert_eq!(db.take_log(), ["parity_outer", "goal parity", "parity(1)", "parity_other"]);

    x.set_value(&mut db).to(3);
    assert_eq!(parity_other(&db, x), 2);
    assert_eq!(db.take_log(), ["parity(3)"]);
}

#[salsa::tracked(returns(copy), lru = 0)]
fn digits(db: &dyn Db, x: Number) -> u32 {
    db.log(format!("digits({})", x.value(db)));
    x.value(db) / 10
}

#[salsa::tracked(returns(copy))]
fn digits_outer(db: &dyn Db, x: Number) -> u32 {
    db.log("digits_outer".to_string());
    shared(db, "digits", || digits(db, x))
}

#[salsa::tracked(returns(copy))]
fn digits_other(db: &dyn Db, x: Number) -> u32 {
    db.log("digits_other".to_string());
    shared(db, "digits", || digits(db, x)) * 100
}

/// As `lru_fingerprint.rs`'s `reader_is_unchanged_when_its_input_has_the_fingerprint_it_read`,
/// with the fingerprint reaching the reader by replay: the capture reads `digits` = 1 with its
/// fingerprint, `digits_other` replays it, and `digits` then changes to 2 and back to 1 (each
/// time computed again after an eviction, so each memo has a fingerprint) while nobody reads
/// `digits_other`. Its replayed edge kept the fingerprint of 1, so it is unchanged.
#[test]
fn a_replayed_edge_keeps_the_fingerprint_it_was_read_with() {
    let mut db = CaptureDb::default();
    let x = Number::new(&db, 12);

    assert_eq!(digits(&db, x), 1);
    assert_eq!(digits::evict_values(&mut db), 1);
    assert_eq!(digits_outer(&db, x), 1);
    assert_eq!(digits_other(&db, x), 100);
    assert_eq!(
        db.take_log(),
        ["digits(12)", "digits_outer", "goal digits", "digits(12)", "digits_other"]
    );

    x.set_value(&mut db).to(20);
    assert_eq!(digits::evict_values(&mut db), 1);
    assert_eq!(digits(&db, x), 2);

    x.set_value(&mut db).to(11);
    assert_eq!(digits::evict_values(&mut db), 1);
    assert_eq!(digits_other(&db, x), 100);
    assert_eq!(db.take_log(), ["digits(20)", "digits(11)"]);
}

#[salsa::tracked]
struct Thing<'db> {
    #[returns(copy)]
    value: u32,
}

#[salsa::accumulator]
#[derive(Debug)]
struct Note(#[allow(dead_code)] u32);

#[salsa::tracked(returns(copy))]
fn unshareable(db: &dyn Db, x: Number) -> u32 {
    let (thing, reads) = salsa::capture_reads(db, || Thing::new(db, x.value(db)));
    db.verdict("tracked struct", reads.is_some());

    let (_, reads) = salsa::capture_reads(db, || Note(x.value(db)).accumulate(db));
    db.verdict("accumulated", reads.is_some());

    let (_, reads) = salsa::capture_reads(db, || db.report_untracked_read());
    db.verdict("untracked read", reads.is_some());

    let (_, reads) = salsa::capture_reads(db, || salsa::unshareable_reads(db));
    db.verdict("unshareable_reads", reads.is_some());

    let (_, reads) = salsa::capture_reads(db, || {
        let (_, inner) = salsa::capture_reads(db, || salsa::unshareable_reads(db));
        assert!(inner.is_none());
        x.value(db)
    });
    db.verdict("unshareable_reads in an inner frame", reads.is_some());

    let (_, reads) = salsa::capture_reads(db, || {
        let (_, inner) = salsa::capture_reads(db, || Note(1).accumulate(db));
        assert!(inner.is_none());
        x.value(db)
    });
    db.verdict("an inner frame accumulated", reads.is_some());

    // What the frames before did is not this frame's.
    let (_, reads) = salsa::capture_reads(db, || x.value(db));
    db.verdict("a plain read after them", reads.is_some());

    thing.value(db)
}

#[test]
fn a_capture_that_may_not_be_shared_is_none() {
    let db = CaptureDb::default();
    let x = Number::new(&db, 3);
    assert_eq!(unshareable(&db, x), 3);
    assert_eq!(
        db.take_verdicts(),
        [
            ("tracked struct", false),
            ("accumulated", false),
            ("untracked read", false),
            ("unshareable_reads", false),
            ("unshareable_reads in an inner frame", false),
            ("an inner frame accumulated", false),
            ("a plain read after them", true),
        ]
    );
}

#[test]
fn a_capture_with_no_active_query_is_none() {
    let db = CaptureDb::default();
    let x = Number::new(&db, 4);
    let (value, reads) = salsa::capture_reads(&db, || x.value(&db));
    assert_eq!(value, 4);
    assert!(reads.is_none());
    // So are these, and they do nothing.
    salsa::unshareable_reads(&db);
}

#[salsa::tracked(returns(copy), cycle_initial = head_initial)]
fn head(db: &dyn Db, x: Number) -> u32 {
    tail(db, x).min(x.value(db))
}

fn head_initial(_db: &dyn Db, _id: salsa::Id, _x: Number) -> u32 {
    0
}

#[salsa::tracked(returns(copy))]
fn tail(db: &dyn Db, x: Number) -> u32 {
    let (value, reads) = salsa::capture_reads(db, || head(db, x));
    db.verdict("provisional", reads.is_some());
    value + 1
}

#[test]
fn a_capture_that_read_a_provisional_value_is_none() {
    let db = CaptureDb::default();
    let x = Number::new(&db, 5);
    assert_eq!(head(&db, x), 5);
    let verdicts = db.take_verdicts();
    assert!(!verdicts.is_empty());
    assert!(
        verdicts.iter().all(|&(_, shareable)| !shareable),
        "{verdicts:?}"
    );
}

#[salsa::tracked(returns(copy))]
fn replay_old(db: &dyn Db, x: Number) -> u32 {
    let reads = db
        .cached_in_any_revision("x")
        .expect("`outer` cached its goal");
    salsa::replay_reads(db, reads);
    x.value(db)
}

#[test]
fn replaying_reads_of_another_revision_panics() {
    let mut db = CaptureDb::default();
    let x = Number::new(&db, 1);
    assert_eq!(outer(&db, x), 11);

    x.set_value(&mut db).to(2);
    let payload = catch_unwind(AssertUnwindSafe(|| replay_old(&db, x)))
        .expect_err("a replay in a later revision must panic");
    let message = payload
        .downcast_ref::<String>()
        .expect("the panic message is formatted");
    assert!(
        message.contains("`replay_reads` of reads captured in revision R1, in revision R2"),
        "{message}"
    );
}

#[salsa::tracked(returns(copy))]
fn ask_y(db: &dyn Db, y: Number) -> u32 {
    db.log("ask_y".to_string());
    shared(db, "y", || y.value(db))
}

/// A goal whose capture holds an inner capture reading `x`, and a replay (or, when nothing is
/// cached this revision, a capture) of the goal reading `y`.
fn nested_goal(db: &dyn Db, x: Number, y: Number) -> u32 {
    shared(db, "nested", || {
        let (tens, inner) = salsa::capture_reads(db, || x.value(db));
        assert!(inner.is_some());
        tens * 10 + shared(db, "y", || y.value(db))
    })
}

#[salsa::tracked(returns(copy))]
fn nest(db: &dyn Db, x: Number, y: Number) -> u32 {
    db.log("nest".to_string());
    nested_goal(db, x, y)
}

#[salsa::tracked(returns(copy))]
fn nest_other(db: &dyn Db, x: Number, y: Number) -> u32 {
    db.log("nest_other".to_string());
    nested_goal(db, x, y) + 1000
}

/// `ask_y` caches the goal on `y`, `nest` computes the nested goal (replaying `y`'s inside it),
/// and `nest_other` replays the nested goal.
fn nested_setup() -> (CaptureDb, Number, Number, Number) {
    let db = CaptureDb::default();
    let x = Number::new(&db, 1);
    let y = Number::new(&db, 2);
    let unrelated = Number::new(&db, 0);
    assert_eq!(ask_y(&db, y), 2);
    assert_eq!(nest(&db, x, y), 12);
    assert_eq!(nest_other(&db, x, y), 1012);
    assert_eq!(
        db.take_log(),
        ["ask_y", "goal y", "nest", "goal nested", "nest_other"]
    );
    (db, x, y, unrelated)
}

#[test]
fn a_replayed_outer_capture_holds_its_inner_capture() {
    let (mut db, x, y, unrelated) = nested_setup();

    unrelated.set_value(&mut db).to(9);
    assert_eq!(nest_other(&db, x, y), 1012);
    assert!(db.take_log().is_empty());

    x.set_value(&mut db).to(3);
    assert_eq!(nest_other(&db, x, y), 1032);
    assert_eq!(db.take_log(), ["nest_other", "goal nested", "goal y"]);
}

#[test]
fn a_replayed_outer_capture_holds_what_it_replayed() {
    let (mut db, x, y, _) = nested_setup();

    y.set_value(&mut db).to(4);
    assert_eq!(nest_other(&db, x, y), 1014);
    assert_eq!(db.take_log(), ["nest_other", "goal nested", "goal y"]);
}

#[salsa::interned]
struct Label<'db> {
    #[returns(copy)]
    key: u32,
}

#[salsa::tracked(returns(copy))]
fn doubled(db: &dyn Db, x: Number) -> u32 {
    x.value(db) * 2
}

/// Captures a read of an input field, of an interned value and of a query, then logs each read
/// `for_each_read` gives as its kind and its ingredient's name.
#[salsa::tracked(returns(copy))]
fn walks_its_reads(db: &dyn Db, x: Number) -> u32 {
    let (value, reads) = salsa::capture_reads(db, || {
        x.value(db) + Label::new(db, 3).key(db) + doubled(db, x)
    });
    let reads = reads.expect("plain reads may be shared");
    let mut seen = 0;
    let walked = reads.for_each_read(db, |_, kind, name| {
        seen += 1;
        db.log(format!("{kind:?} {name}"));
        std::ops::ControlFlow::Continue(())
    });
    assert_eq!(walked, std::ops::ControlFlow::Continue(()));
    assert_eq!(seen, reads.len());
    value
}

#[test]
fn for_each_read_gives_each_read_with_its_kind_and_name_in_read_order() {
    let db = CaptureDb::default();
    let x = Number::new(&db, 1);
    assert_eq!(walks_its_reads(&db, x), 1 + 3 + 2);
    let log = db.take_log();
    // An interned value's read records an edge only when its slot can be reused.
    let without_interned: Vec<&str> = log
        .iter()
        .filter(|entry| !entry.starts_with("Interned"))
        .map(String::as_str)
        .collect();
    assert_eq!(without_interned.len(), 2, "{log:?}");
    assert!(without_interned[0].starts_with("Input ") && without_interned[0].contains("value"), "{log:?}");
    assert!(without_interned[1].starts_with("Query ") && without_interned[1].contains("doubled"), "{log:?}");
    for entry in log.iter().filter(|entry| entry.starts_with("Interned")) {
        assert!(entry.contains("Label"), "{log:?}");
    }
    let interned = log.iter().position(|entry| entry.starts_with("Interned"));
    if let Some(at) = interned {
        assert_eq!(at, 1, "the interned read comes between the two others: {log:?}");
    }
}

#[salsa::tracked(returns(copy))]
fn added(db: &dyn Db, x: Number, by: u32) -> u32 {
    x.value(db) + by
}

/// Captures a read of `added(x, 5)`, a tracked fn of two arguments, and reads its arguments back
/// from the read's key: as `(Number, u32)`, and not as a tuple they are not.
#[salsa::tracked(returns(copy))]
fn reads_its_arguments(db: &dyn Db, x: Number) -> u32 {
    let (value, reads) = salsa::capture_reads(db, || added(db, x, 5));
    let reads = reads.expect("plain reads may be shared");
    let mut found = 0;
    let walked = reads.for_each_read(db, |key, kind, _| {
        if kind == salsa::ReadKind::Query {
            let key = key.key_index();
            assert!(salsa::plumbing::interned_arguments::<(Number, u32)>(db, key) == Some((x, 5)));
            assert!(salsa::plumbing::interned_arguments::<(u32, Number)>(db, key).is_none());
            found += 1;
        }
        std::ops::ControlFlow::Continue(())
    });
    assert_eq!(walked, std::ops::ControlFlow::Continue(()));
    assert_eq!(found, 1);
    value
}

#[test]
fn interned_arguments_reads_a_query_reads_arguments_back() {
    let db = CaptureDb::default();
    let x = Number::new(&db, 1);
    assert_eq!(reads_its_arguments(&db, x), 6);
    // A key of an input names no interned arguments.
    assert!(salsa::plumbing::interned_arguments::<(Number, u32)>(&db, salsa::plumbing::AsId::as_id(&x)).is_none());
}
