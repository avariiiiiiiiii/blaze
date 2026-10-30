use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        Arc, Barrier, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use blaze_core::{Completion, Done, Effect, FileName, Mapping, Tag};
use blaze_format::{FormatError, Persist, Record, SchemaHash, Writer};
use blaze_ids::{ContentId, Fingerprint};
use blaze_vfs::{View, ViewId};

use crate::{
    Cancelled, Changed, Config, Cx, Database, DbError, Edge, Input, MemTree, Query, QueryKey,
    QueryKind, Stored, table::Aligned,
};

#[derive(Clone, Copy)]
#[repr(C)]
struct Word(u64);

const WORD: SchemaHash = SchemaHash(ContentId([0x57; 32]));

// SAFETY: a `u64` in a `repr(C)` struct: no padding, no pointer, every bit pattern valid.
unsafe impl Record for Word {
    const SCHEMA: SchemaHash = WORD;
    fn check(_: &[u8], _: &[u8]) -> Result<(), FormatError> {
        Ok(())
    }
}

struct Num(u64);

impl Persist for Num {
    const SCHEMA: SchemaHash = WORD;
    type Ref<'a> = u64;
    fn write(&self, out: &mut Writer) -> u64 {
        out.push(&Word(self.0))
    }
    fn read(buffer: &[u8]) -> Result<u64, FormatError> {
        blaze_format::open::<Word>(buffer).map(|word| word.0)
    }
}

thread_local! {
    static RUNS: RefCell<HashMap<&'static str, usize>> = RefCell::new(HashMap::new());
}

fn ran(name: &'static str) {
    RUNS.with(|runs| *runs.borrow_mut().entry(name).or_default() += 1);
}

fn runs(name: &'static str) -> usize {
    RUNS.with(|runs| runs.borrow().get(name).copied().unwrap_or(0))
}

fn reset() {
    RUNS.with(|runs| runs.borrow_mut().clear());
}

fn key_id(key: &str) -> ContentId {
    ContentId::of("blaze v2 db test key 1", key.as_bytes())
}

/// The length of a file, `u64::MAX` for none.
struct Len;

impl Query for Len {
    const KIND: QueryKind = QueryKind(0x2f00);
    const NAME: &'static str = "len";
    const PRODUCER: ContentId = ContentId([1; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        ran("len");
        Ok(Num(match cx.file(key)? {
            Some(file) => file.bytes.len() as u64,
            None => u64::MAX,
        }))
    }
}

/// Twice [`Len`].
struct Double;

impl Query for Double {
    const KIND: QueryKind = QueryKind(0x2f01);
    const NAME: &'static str = "double";
    const PRODUCER: ContentId = ContentId([2; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        ran("double");
        Ok(Num(cx.get::<Len>(key)?.wrapping_mul(2)))
    }
}

/// The file names another file; 1 + [`Must`] of it, 0 for an empty file.
struct Follow;

impl Query for Follow {
    const KIND: QueryKind = QueryKind(0x2f02);
    const NAME: &'static str = "follow";
    const PRODUCER: ContentId = ContentId([3; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        ran("follow");
        let name = match cx.file(key)? {
            Some(file) => String::from_utf8(file.bytes.to_vec()).expect("a name"),
            None => String::new(),
        };
        if name.is_empty() {
            return Ok(Num(0));
        }
        Ok(Num(1 + cx.get::<Must>(&name)?))
    }
}

/// The length of a file that must be there: asked of one that is gone, it panics, as a query
/// asked of an item that no longer exists does (#350's `AstIdMap` panic).
struct Must;

impl Query for Must {
    const KIND: QueryKind = QueryKind(0x2f03);
    const NAME: &'static str = "must";
    const PRODUCER: ContentId = ContentId([4; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        ran("must");
        let file = cx.file(key)?.unwrap_or_else(|| panic!("must was asked of {key}, which is gone"));
        Ok(Num(file.bytes.len() as u64))
    }
}

static GATE: AtomicUsize = AtomicUsize::new(0);
static BARRIER: OnceLock<Barrier> = OnceLock::new();

/// The file names another; 1 + [`Chain`] of it, 0 for an empty file. Its first two runs wait
/// for each other, to make two runs hold a key each before asking the other's.
struct Chain;

impl Query for Chain {
    const KIND: QueryKind = QueryKind(0x2f04);
    const NAME: &'static str = "chain";
    const PRODUCER: ContentId = ContentId([5; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        if GATE.fetch_add(1, Ordering::SeqCst) < 2 {
            BARRIER.get_or_init(|| Barrier::new(2)).wait();
        }
        let name = match cx.file(key)? {
            Some(file) => String::from_utf8(file.bytes.to_vec()).expect("a name"),
            None => String::new(),
        };
        if name.is_empty() {
            return Ok(Num(0));
        }
        Ok(Num(1 + cx.get::<Chain>(&name)?))
    }
}

/// The length of the view's configuration.
struct Conf;

impl Query for Conf {
    const KIND: QueryKind = QueryKind(0x2f05);
    const NAME: &'static str = "conf";
    const PRODUCER: ContentId = ContentId([6; 32]);
    type Key = ();
    type Value = Num;
    fn input(_: &()) -> ContentId {
        key_id("")
    }
    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Num, Cancelled> {
        ran("conf");
        Ok(Num(cx.config()?.len() as u64))
    }
}

/// How many names a directory has.
struct Names;

impl Query for Names {
    const KIND: QueryKind = QueryKind(0x2f06);
    const NAME: &'static str = "names";
    const PRODUCER: ContentId = ContentId([7; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        ran("names");
        Ok(Num(cx.dir(key)?.map_or(u64::MAX, |names| names.len() as u64)))
    }
}

const FACT: QueryKey = QueryKey { kind: QueryKind(0x1fff), input: ContentId([9; 32]) };

/// Reads a fact no query answers, as an index the caller holds would.
struct ReadsFact;

impl Query for ReadsFact {
    const KIND: QueryKind = QueryKind(0x2f07);
    const NAME: &'static str = "reads fact";
    const PRODUCER: ContentId = ContentId([8; 32]);
    type Key = ();
    type Value = Num;
    fn input(_: &()) -> ContentId {
        key_id("")
    }
    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Num, Cancelled> {
        ran("reads fact");
        cx.read(FACT, Fingerprint(1));
        Ok(Num(1))
    }
}

const TAGS: u64 = 1 << 40;

fn config(tree: &Arc<MemTree>) -> Config {
    Config { tree: Some(tree.clone()), dir: FileName::new("db"), tags: TAGS }
}

fn register(db: &mut Database) {
    db.register::<Len>();
    db.register::<Double>();
    db.register::<Follow>();
    db.register::<Must>();
    db.register::<Chain>();
    db.register::<Conf>();
    db.register::<Names>();
    db.register::<ReadsFact>();
}

fn setup() -> (Database, Arc<MemTree>) {
    reset();
    let tree = Arc::new(MemTree::default());
    let mut db = Database::new(config(&tree));
    register(&mut db);
    (db, tree)
}

fn view(db: &Database, tree: &MemTree, files: &[(&str, &str)]) -> ViewId {
    let files: Vec<(&str, &[u8])> = files.iter().map(|(path, bytes)| (*path, bytes.as_bytes())).collect();
    db.open(View { root: tree.root(&files) })
}

fn key<Q: Query<Key = String>>(name: &str) -> QueryKey {
    QueryKey { kind: Q::KIND, input: Q::input(&name.to_owned()) }
}

fn double(db: &Database, view: ViewId, name: &str) -> u64 {
    db.get::<Double>(view, &name.to_owned()).expect("double")
}

fn mapping(bytes: &[u8]) -> Mapping {
    Mapping(Arc::new(Aligned::copy(bytes)))
}

/// Takes the one write in `effects`.
fn written(effects: &mut Vec<Effect>) -> (FileName, Vec<u8>, Tag) {
    assert_eq!(effects.len(), 1, "{effects:?}");
    match effects.pop() {
        Some(Effect::Write { file, bytes, tag }) => (file, bytes, tag),
        other => panic!("not a write: {other:?}"),
    }
}

fn mapped(effects: &mut Vec<Effect>) -> (FileName, Tag) {
    assert_eq!(effects.len(), 1, "{effects:?}");
    match effects.pop() {
        Some(Effect::Map { file, tag }) => (file, tag),
        other => panic!("not a map: {other:?}"),
    }
}

#[test]
fn equal_inputs_share_results_across_views() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "xyz")]);
    assert_eq!(double(&db, one, "a"), 6);
    assert_eq!((runs("len"), runs("double")), (1, 1));

    let other_files = view(&db, &tree, &[("a", "xyz"), ("b", "else")]);
    assert_eq!(double(&db, other_files, "a"), 6);
    assert_eq!((runs("len"), runs("double")), (1, 1), "a's inputs are equal: nothing reruns");

    let shorter = view(&db, &tree, &[("a", "xy")]);
    assert_eq!(double(&db, shorter, "a"), 4);
    assert_eq!((runs("len"), runs("double")), (2, 2));

    assert_eq!(double(&db, one, "a"), 6, "the first view still has its own result");
    let again = view(&db, &tree, &[("a", "xyz")]);
    assert_eq!(double(&db, again, "a"), 6);
    assert_eq!((runs("len"), runs("double")), (2, 2), "both results are kept at once");
}

#[test]
fn an_equal_fingerprint_stops_the_rerun() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    assert_eq!(double(&db, one, "a"), 6);
    let edited = view(&db, &tree, &[("a", "xyz")]);
    assert_eq!(double(&db, edited, "a"), 6);
    assert_eq!(runs("len"), 2, "the file changed: its length is asked again");
    assert_eq!(runs("double"), 1, "its length did not: early cutoff");
}

#[test]
fn verification_stops_at_the_first_changed_edge() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "b"), ("b", "12")]);
    assert_eq!(db.get::<Follow>(one, &"a".to_owned()), Ok(3));
    // `b` is gone and `a` names `c`: verifying follow(a) must stop at `a`, never ask must(b).
    let two = view(&db, &tree, &[("a", "c"), ("c", "1")]);
    assert_eq!(db.get::<Follow>(two, &"a".to_owned()), Ok(2));
    assert_eq!(runs("must"), 2);
    assert_eq!(runs("follow"), 2);
}

#[test]
fn why_names_the_chain_back_to_the_input() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let first = db.why(one, &key::<Double>("a")).expect("it ran");
    assert!(first.chain.is_empty(), "a first run has nothing that changed: {first:?}");

    let longer = view(&db, &tree, &[("a", "abcd")]);
    double(&db, longer, "a");
    let why = db.why(longer, &key::<Double>("a")).expect("it reran");
    let keys: Vec<QueryKey> = why.chain.iter().map(|changed| changed.key).collect();
    assert_eq!(keys, vec![key::<Len>("a"), Input::File("a".to_owned()).key()]);
    assert_eq!(why.input, Some(Input::File("a".to_owned())));
    let Changed { was, now, .. } = why.chain[0];
    assert_ne!(was, now);

    let same = view(&db, &tree, &[("a", "abc")]);
    double(&db, same, "a");
    assert_eq!(db.why(same, &key::<Double>("a")), None, "verified, not run");
}

#[test]
fn an_evicted_value_is_made_again_and_cutoff_holds() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    assert_eq!(db.evict(&key::<Len>("a")), 1);
    assert_eq!(db.memo(&key::<Len>("a")).map(|memo| memo.value), Some(Stored::Evicted));

    let edited = view(&db, &tree, &[("a", "xyz")]);
    assert_eq!(double(&db, edited, "a"), 6);
    assert_eq!(runs("double"), 1, "eviction kept len's fingerprint, so double is cut off");

    let same = view(&db, &tree, &[("a", "abc")]);
    assert_eq!(db.get::<Len>(same, &"a".to_owned()), Ok(3));
    assert_eq!(runs("len"), 3, "the evicted value was made again");
}

#[test]
fn collection_drops_what_nothing_reaches() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let mut effects = Vec::new();
    db.collect(&[], &mut effects);
    assert!(db.memo(&key::<Double>("a")).is_some(), "an open view reaches it");
    assert!(effects.is_empty(), "what an open view uses stays in memory: {effects:?}");

    db.close(one);
    db.collect(&[key::<Double>("a")], &mut effects);
    assert!(db.memo(&key::<Double>("a")).is_some(), "kept");
    assert!(db.memo(&key::<Len>("a")).is_some(), "reached from a kept key");

    effects.clear();
    db.collect(&[], &mut effects);
    assert_eq!(db.memo(&key::<Double>("a")), None);
    assert_eq!(db.memo(&key::<Len>("a")), None);
}

#[test]
fn cold_values_move_to_disk_and_read_from_it() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    db.close(one);
    let mut effects = Vec::new();
    db.collect(&[key::<Double>("a")], &mut effects);
    let (file, bytes, tag) = written(&mut effects);
    assert_eq!(file.as_str(), "db/cold/0");
    assert!(db.owns(tag));
    db.complete(Completion { tag, done: Done::Written }, &mut effects);
    let (file, tag) = mapped(&mut effects);
    assert_eq!(file.as_str(), "db/cold/0");
    db.complete(Completion { tag, done: Done::Mapped(mapping(&bytes)) }, &mut effects);
    assert!(matches!(db.memo(&key::<Double>("a")).map(|memo| memo.value), Some(Stored::Disk(_))));

    let again = view(&db, &tree, &[("a", "abc")]);
    assert_eq!(double(&db, again, "a"), 6);
    assert_eq!(runs("double"), 1, "read from the cold pack, not made again");
}

#[test]
fn the_image_holds_the_database() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let mut effects = Vec::new();
    db.write_image(&mut effects);
    let (file, bytes, _) = written(&mut effects);
    assert_eq!(file.as_str(), "db/image");

    let mut started = Database::from_image_with(config(&tree), mapping(&bytes)).expect("an image");
    register(&mut started);
    reset();
    let same = view(&started, &tree, &[("a", "abc")]);
    assert_eq!(double(&started, same, "a"), 6);
    assert_eq!((runs("len"), runs("double")), (0, 0), "every memo verified from the image");
    assert!(matches!(started.memo(&key::<Double>("a")).map(|memo| memo.value), Some(Stored::Disk(_))));

    let longer = view(&started, &tree, &[("a", "abcd")]);
    assert_eq!(double(&started, longer, "a"), 8);
    assert_eq!((runs("len"), runs("double")), (1, 1));

    let mut again = Vec::new();
    db.write_image(&mut again);
    assert_eq!(written(&mut again).1, bytes, "the same database writes the same image");
}

#[test]
fn an_image_of_another_schema_is_refused() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let mut effects = Vec::new();
    db.write_image(&mut effects);
    let (_, mut bytes, _) = written(&mut effects);
    bytes[8] ^= 1;
    assert!(matches!(
        Database::from_image(mapping(&bytes)),
        Err(DbError::Image(FormatError::Schema { .. }))
    ));
    bytes[8] ^= 1;
    bytes.truncate(bytes.len() - 1);
    assert!(matches!(Database::from_image(mapping(&bytes)), Err(DbError::Image(FormatError::Header(_)))));
}

#[test]
fn settling_swaps_to_the_mapped_image() {
    let (mut db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let mut effects = Vec::new();
    db.write_image(&mut effects);
    let (_, bytes, tag) = written(&mut effects);
    assert_eq!(db.settle(&mut effects), Ok(false), "nothing mapped yet");
    db.complete(Completion { tag, done: Done::Written }, &mut effects);
    let (file, tag) = mapped(&mut effects);
    assert_eq!(file.as_str(), "db/image");
    db.complete(Completion { tag, done: Done::Mapped(mapping(&bytes)) }, &mut effects);
    assert!(db.settled());
    assert_eq!(db.settle(&mut effects), Ok(true));
    assert!(effects.is_empty(), "no cold packs to remove: {effects:?}");

    assert_eq!(double(&db, one, "a"), 6, "the view stays open across the swap");
    assert_eq!((runs("len"), runs("double")), (1, 1));
    assert!(matches!(db.memo(&key::<Double>("a")).map(|memo| memo.value), Some(Stored::Disk(_))));
    db.reclaim();
}

#[test]
fn a_wait_that_would_close_a_cycle_is_cancelled() {
    let (db, tree) = setup();
    let x = view(&db, &tree, &[("a", "b"), ("b", "")]);
    let y = view(&db, &tree, &[("a", ""), ("b", "a")]);
    let cancelled = AtomicUsize::new(0);
    let ask = |view: ViewId, name: &str| loop {
        match db.get::<Chain>(view, &name.to_owned()) {
            Ok(n) => break n,
            Err(DbError::Cancelled) => {
                cancelled.fetch_add(1, Ordering::SeqCst);
            }
            Err(error) => panic!("{error}"),
        }
    };
    let (in_x, in_y) = std::thread::scope(|scope| {
        let in_x = scope.spawn(|| ask(x, "a"));
        let in_y = scope.spawn(|| ask(y, "b"));
        (in_x.join().expect("x"), in_y.join().expect("y"))
    });
    assert_eq!((in_x, in_y), (1, 1));
    assert!(cancelled.load(Ordering::SeqCst) >= 1, "one of the two waits closed a cycle");
}

#[test]
fn configuration_and_directories_are_inputs() {
    let (db, tree) = setup();
    let root = tree.root(&[("src/a.rs", b"1".as_slice()), ("src/b.rs", b"2")]);
    let x86 = db.open_with(View { root }, b"x86_64");
    let arm = db.open_with(View { root }, b"aarch64");
    assert_eq!(db.get::<Conf>(x86, &()), Ok(6));
    assert_eq!(db.get::<Conf>(arm, &()), Ok(7));
    let x86_again = db.open_with(View { root }, b"x86_64");
    assert_eq!(db.get::<Conf>(x86_again, &()), Ok(6));
    assert_eq!(runs("conf"), 2);

    assert_eq!(db.get::<Names>(x86, &"src".to_owned()), Ok(2));
    let edited = view(&db, &tree, &[("src/a.rs", "11"), ("src/b.rs", "2")]);
    assert_eq!(db.get::<Names>(edited, &"src".to_owned()), Ok(2));
    assert_eq!(runs("names"), 1, "an edit inside the directory leaves its names");
    let added = view(&db, &tree, &[("src/a.rs", "1"), ("src/b.rs", "2"), ("src/c.rs", "3")]);
    assert_eq!(db.get::<Names>(added, &"src".to_owned()), Ok(3));
    assert_eq!(db.get::<Names>(added, &"nowhere".to_owned()), Ok(u64::MAX));
}

#[test]
fn a_fact_no_query_answers_is_never_reused_unverified() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "x")]);
    assert_eq!(db.get::<ReadsFact>(one, &()), Ok(1));
    assert_eq!(db.get::<ReadsFact>(one, &()), Ok(1));
    assert_eq!(runs("reads fact"), 1, "within a view it is resolved once");
    let two = view(&db, &tree, &[("a", "x")]);
    assert_eq!(db.get::<ReadsFact>(two, &()), Ok(1));
    assert_eq!(runs("reads fact"), 2, "another view can't verify the fact, so it runs again");
}

#[test]
fn asking_what_is_not_there_is_an_error() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "x")]);
    db.close(one);
    assert_eq!(db.get::<Len>(one, &"a".to_owned()), Err(DbError::NoView(one)));
    let empty = Database::empty();
    let none = empty.open(View { root: ContentId([0; 32]) });
    assert_eq!(empty.get::<Len>(none, &"a".to_owned()), Err(DbError::Unknown(Len::KIND)));
}

#[test]
fn edges_round_trip_through_their_record() {
    let edge = Edge {
        read: QueryKey { kind: QueryKind(0x1234), input: ContentId([0xab; 32]) },
        saw: Fingerprint(0x0123_4567_89ab_cdef_0011_2233_4455_6677),
    };
    let bytes = crate::image::encode_edge(&edge);
    assert_eq!(bytes.len(), crate::table::EDGE_RECORD);
    assert_eq!(crate::table::decode_edge(&bytes), edge);
}
