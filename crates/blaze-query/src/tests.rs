use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        Arc, Barrier, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use blaze_core::{Completion, Done, Effect, FileName, Mapping, Tag};
use blaze_format::{FormatError, Persist, Record, SchemaHash, Slice, Writer};
use blaze_ids::{ContentId, Fingerprint};
use blaze_image::{Builder, Cause, Corrupt, Image, Refused, SectionId};
use blaze_vfs::{Bytes, Mode, View, ViewId};
use intern::Live;

use crate::{
    Cancelled, Changed, Config, Cx, Database, DbError, Edge, Input, Marks, MemTree, Query,
    QueryKey, QueryKind, Stored, Tree,
    table::{Aligned, EdgeRecord},
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
    db.pin().get::<Double>(view, &name.to_owned()).expect("double")
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
    assert_eq!(db.pin().get::<Follow>(one, &"a".to_owned()), Ok(3));
    // `b` is gone and `a` names `c`: verifying follow(a) must stop at `a`, never ask must(b).
    let two = view(&db, &tree, &[("a", "c"), ("c", "1")]);
    assert_eq!(db.pin().get::<Follow>(two, &"a".to_owned()), Ok(2));
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
    assert_eq!(db.pin().get::<Len>(same, &"a".to_owned()), Ok(3));
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

/// The database's sections of an image, as the daemon writes them (it adds the symbol table
/// and its own sections, which the database never reads).
fn image_of(db: &Database, builder: Builder) -> (Vec<u8>, Live) {
    let mut builder = builder;
    let mut live = Live::new();
    db.write_image(&mut builder, &mut live).expect("every kind is registered");
    (builder.finish().expect("a small image"), live)
}

fn opened(bytes: &[u8]) -> Arc<Image<Mapping>> {
    Arc::new(Image::open(mapping(bytes)).expect("an image this blaze wrote"))
}

#[test]
fn the_image_holds_the_database() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let (bytes, _) = image_of(&db, Builder::new());

    let mut started = Database::open_image(config(&tree), opened(&bytes)).expect("an image");
    register(&mut started);
    reset();
    let same = view(&started, &tree, &[("a", "abc")]);
    assert_eq!(double(&started, same, "a"), 6);
    assert_eq!((runs("len"), runs("double")), (0, 0), "every memo verified from the image");
    assert!(matches!(started.memo(&key::<Double>("a")).map(|memo| memo.value), Some(Stored::Disk(_))));

    let longer = view(&started, &tree, &[("a", "abcd")]);
    assert_eq!(double(&started, longer, "a"), 8);
    assert_eq!((runs("len"), runs("double")), (1, 1));
    assert_eq!(started.take_corrupt(), vec![], "nothing read was corrupt");

    assert_eq!(image_of(&db, Builder::new()).0, bytes, "the same database writes the same image");
}

/// Values name symbols by their IDs in the image's symbol table, so an image is used only
/// after the table was restored from it.
#[test]
fn an_image_whose_symbols_were_not_restored_is_refused() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let (bytes, _) = image_of(&db, Builder::new());
    assert_eq!(intern::restored_from(), None, "no test restores the process's symbols");
    assert_eq!(Database::from_image_with(config(&tree), opened(&bytes)).err(), Some(DbError::Symbols));
}

#[test]
fn an_image_without_the_databases_sections_is_refused() {
    let empty = Builder::new().finish().expect("an image");
    assert!(matches!(
        Database::open_image(Config::memory(), opened(&empty)),
        Err(DbError::Refused(Refused::Missing { id: SectionId::DB_MEMOS }))
    ));
    let mut other = Builder::new();
    other.add(SectionId::DB_MEMOS, blaze_format::schema("blaze v2 db memos 1"), &[]);
    let other = other.finish().expect("an image");
    let error = Database::open_image(Config::memory(), opened(&other)).err();
    assert!(matches!(error, Some(DbError::Refused(Refused::Schema { id: SectionId::DB_MEMOS, .. }))), "{error:?}");
    assert!(error.is_some_and(|error| error.is_image()));
}

#[test]
fn a_kind_with_memos_must_be_registered_to_write_an_image() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let (bytes, _) = image_of(&db, Builder::new());
    let started = Database::open_image(config(&tree), opened(&bytes)).expect("an image");
    let mut live = Live::new();
    let error = started.write_image(&mut Builder::new(), &mut live).err();
    assert!(matches!(error, Some(DbError::Unknown(_))), "{error:?}");
}

#[test]
fn settling_swaps_to_the_mapped_image() {
    let (mut db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let (bytes, _) = image_of(&db, Builder::new());
    let mut effects = Vec::new();
    assert_eq!(db.settle(opened(&bytes), &mut effects), Ok(()));
    assert!(effects.is_empty(), "no cold packs to remove: {effects:?}");

    assert_eq!(double(&db, one, "a"), 6, "the view stays open across the swap");
    assert_eq!((runs("len"), runs("double")), (1, 1));
    assert!(matches!(db.memo(&key::<Double>("a")).map(|memo| memo.value), Some(Stored::Disk(_))));
    db.reclaim();
}

/// One byte flipped in a group of a database section: the group is reported once, by section
/// and group; the keys whose memos it held are computed again and every other key is used from
/// the image. A group holding an input's path is read at start, so the database starts empty
/// naming it.
#[test]
fn a_corrupt_group_drops_only_the_memos_in_it() {
    const KEYS: usize = 300;
    let (db, tree) = setup();
    let names: Vec<String> = (0..KEYS).map(|n| format!("f{n}")).collect();
    let contents: Vec<String> = (0..KEYS).map(|n| "x".repeat(n % 7)).collect();
    let files: Vec<(&str, &str)> = names.iter().map(String::as_str).zip(contents.iter().map(String::as_str)).collect();
    let one = view(&db, &tree, &files);
    for name in &names {
        double(&db, one, name);
    }
    // Groups of 4 KiB, so each section spans several.
    let (bytes, _) = image_of(&db, Builder::with_group_shift(12));
    let sections = opened(&bytes).sections().to_vec();
    let mut tried = 0;
    for section in sections.iter().filter(|it| it.id != SectionId::DB_INPUTS) {
        for group in 0..section.groups {
            let mut flipped = bytes.clone();
            flipped[(section.at + u64::from(group) * 4096) as usize] ^= 0x40;
            let image = opened(&flipped);
            let mut started = match Database::open_image(config(&tree), image) {
                Ok(started) => started,
                Err(DbError::Corrupt(corrupt)) => {
                    assert_eq!((corrupt.section, corrupt.group), (SectionId::DB_BLOB, group), "only paths are read at start");
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            register(&mut started);
            reset();
            let same = view(&started, &tree, &files);
            for (n, name) in names.iter().enumerate() {
                assert_eq!(double(&started, same, name), 2 * (n % 7) as u64, "{name} is right whatever was dropped");
            }
            // Writing the image reads every key's memos, so every group is read by now.
            let _ = image_of(&started, Builder::new());
            let found = started.take_corrupt();
            assert_eq!(
                found.iter().map(|it| (it.section, it.group)).collect::<Vec<_>>(),
                vec![(section.id, group)],
                "{} group {group}",
                section.id
            );
            assert_eq!(started.take_corrupt(), vec![], "each group is reported once");
            let reran = runs("len") + runs("double");
            assert!(reran > 0, "{} group {group} held memos, computed again", section.id);
            assert!(reran < KEYS, "{} group {group}: the other keys' memos are used, {reran} reran", section.id);
            tried += 1;
        }
    }
    assert!(tried > 4 * 2, "every database section spans groups: {tried} tried");
}

#[test]
fn a_wait_that_would_close_a_cycle_is_cancelled() {
    let (db, tree) = setup();
    let x = view(&db, &tree, &[("a", "b"), ("b", "")]);
    let y = view(&db, &tree, &[("a", ""), ("b", "a")]);
    let cancelled = AtomicUsize::new(0);
    let ask = |view: ViewId, name: &str| loop {
        match db.pin().get::<Chain>(view, &name.to_owned()) {
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
    assert_eq!(db.pin().get::<Conf>(x86, &()), Ok(6));
    assert_eq!(db.pin().get::<Conf>(arm, &()), Ok(7));
    let x86_again = db.open_with(View { root }, b"x86_64");
    assert_eq!(db.pin().get::<Conf>(x86_again, &()), Ok(6));
    assert_eq!(runs("conf"), 2);

    assert_eq!(db.pin().get::<Names>(x86, &"src".to_owned()), Ok(2));
    let edited = view(&db, &tree, &[("src/a.rs", "11"), ("src/b.rs", "2")]);
    assert_eq!(db.pin().get::<Names>(edited, &"src".to_owned()), Ok(2));
    assert_eq!(runs("names"), 1, "an edit inside the directory leaves its names");
    let added = view(&db, &tree, &[("src/a.rs", "1"), ("src/b.rs", "2"), ("src/c.rs", "3")]);
    assert_eq!(db.pin().get::<Names>(added, &"src".to_owned()), Ok(3));
    assert_eq!(db.pin().get::<Names>(added, &"nowhere".to_owned()), Ok(u64::MAX));
}

#[test]
fn a_fact_no_query_answers_is_never_reused_unverified() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "x")]);
    assert_eq!(db.pin().get::<ReadsFact>(one, &()), Ok(1));
    assert_eq!(db.pin().get::<ReadsFact>(one, &()), Ok(1));
    assert_eq!(runs("reads fact"), 1, "within a view it is resolved once");
    let two = view(&db, &tree, &[("a", "x")]);
    assert_eq!(db.pin().get::<ReadsFact>(two, &()), Ok(1));
    assert_eq!(runs("reads fact"), 2, "another view can't verify the fact, so it runs again");
}

#[test]
fn asking_what_is_not_there_is_an_error() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "x")]);
    db.close(one);
    assert_eq!(db.pin().get::<Len>(one, &"a".to_owned()), Err(DbError::NoView(one)));
    let empty = Database::empty();
    let none = empty.open(View { root: ContentId([0; 32]) });
    assert_eq!(empty.pin().get::<Len>(none, &"a".to_owned()), Err(DbError::Unknown(Len::KIND)));
}

#[test]
fn the_database_is_shared_by_reference() {
    fn shared<T: Send + Sync>() {}
    shared::<Database>();
}

static HELD: AtomicUsize = AtomicUsize::new(0);
static RELEASED: AtomicUsize = AtomicUsize::new(0);
static ASKING: AtomicUsize = AtomicUsize::new(0);

/// Runs until [`RELEASED`] is set, holding its key.
struct Hold;

impl Query for Hold {
    const KIND: QueryKind = QueryKind(0x2f08);
    const NAME: &'static str = "hold";
    const PRODUCER: ContentId = ContentId([10; 32]);
    type Key = ();
    type Value = Num;
    fn input(_: &()) -> ContentId {
        key_id("hold")
    }
    fn execute(_: &mut Cx<'_>, _: &()) -> Result<Num, Cancelled> {
        HELD.store(1, Ordering::SeqCst);
        while RELEASED.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(Num(5))
    }
}

/// 1 + [`Hold`]: a query that waits, inside its run, for a key another task runs.
struct AsksHold;

impl Query for AsksHold {
    const KIND: QueryKind = QueryKind(0x2f09);
    const NAME: &'static str = "asks hold";
    const PRODUCER: ContentId = ContentId([11; 32]);
    type Key = ();
    type Value = Num;
    fn input(_: &()) -> ContentId {
        key_id("asks hold")
    }
    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Num, Cancelled> {
        ASKING.store(1, Ordering::SeqCst);
        Ok(Num(1 + cx.get::<Hold>(&())?))
    }
}

/// A query waiting on a worker pumps the pool: a job pumped above it may ask the database (it is
/// a task of its own, not the query), and one that asks the key the query below it holds is
/// cancelled at once rather than waiting for a run that can't go on until it returns.
#[test]
fn a_job_pumped_under_a_waiting_query_asks_as_a_task_of_its_own() {
    use blaze_graph::{pool::Pool, topology::Topology};
    use std::{sync::mpsc, time::Duration};

    let tree = Arc::new(MemTree::default());
    let mut db = Database::new(config(&tree));
    register(&mut db);
    db.register::<Hold>();
    db.register::<AsksHold>();
    let db = Arc::new(db);
    let one = view(&db, &tree, &[("a", "abc")]);
    let pool = Pool::new(&Topology::flat(1), false).expect("a pool");
    let wait = |flag: &AtomicUsize| {
        let until = std::time::Instant::now() + Duration::from_secs(60);
        while flag.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < until, "timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    };

    std::thread::scope(|scope| {
        let holder = scope.spawn(|| db.pin().get::<Hold>(one, &()));
        wait(&HELD);

        let (asked, heard) = mpsc::channel();
        {
            let (db, asked) = (db.clone(), asked.clone());
            pool.submit(None, move || db.pin().get::<AsksHold>(one, &()), move |it| _ = asked.send(("asks", it)));
        }
        wait(&ASKING);
        std::thread::sleep(Duration::from_millis(50));

        {
            let (db, asked) = (db.clone(), asked.clone());
            pool.submit(
                None,
                move || {
                    let depth = blaze_graph::pump::depth() as u64;
                    db.pin().get::<Len>(one, &"a".to_owned()).map(|len| len * 100 + depth)
                },
                move |it| _ = asked.send(("len", it)),
            );
        }
        let (name, len) = heard.recv_timeout(Duration::from_secs(60)).expect("the pumped job ran");
        assert_eq!(name, "len", "the one worker is the waiting query's: only a pump runs this");
        assert_eq!(len.expect("no panic"), Ok(301), "len 3, asked from pump depth 1");

        {
            let (db, asked) = (db.clone(), asked.clone());
            pool.submit(None, move || db.pin().get::<AsksHold>(one, &()), move |it| _ = asked.send(("again", it)));
        }
        let (name, again) = heard.recv_timeout(Duration::from_secs(60)).expect("not a deadlock");
        assert_eq!(name, "again");
        assert_eq!(again.expect("no panic"), Err(DbError::Cancelled));

        RELEASED.store(1, Ordering::SeqCst);
        assert_eq!(holder.join().expect("the holder"), Ok(5));
        let (name, asks) = heard.recv_timeout(Duration::from_secs(60)).expect("the waiting query ended");
        assert_eq!(name, "asks");
        assert_eq!(asks.expect("no panic"), Ok(6));
    });
}

static COUNTED: AtomicUsize = AtomicUsize::new(0);

/// The length of a file, its runs counted across threads.
struct Counted;

impl Query for Counted {
    const KIND: QueryKind = QueryKind(0x2f0a);
    const NAME: &'static str = "counted";
    const PRODUCER: ContentId = ContentId([12; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        COUNTED.fetch_add(1, Ordering::SeqCst);
        Ok(Num(cx.file(key)?.map_or(u64::MAX, |file| file.bytes.len() as u64)))
    }
}

#[test]
fn threads_asking_overlapping_keys_run_each_once() {
    let (mut db, tree) = setup();
    db.register::<Counted>();
    let files: Vec<(String, String)> = (0..64).map(|i| (format!("f{i}"), "x".repeat(i))).collect();
    let files: Vec<(&str, &str)> = files.iter().map(|(path, bytes)| (path.as_str(), bytes.as_str())).collect();
    let one = view(&db, &tree, &files);
    std::thread::scope(|scope| {
        for thread in 0..8 {
            let db = &db;
            scope.spawn(move || {
                let pinned = db.pin();
                for round in 0..64 {
                    let i = (round * 7 + thread * 5) % 64;
                    assert_eq!(pinned.get::<Counted>(one, &format!("f{i}")), Ok(i as u64));
                }
            });
        }
    });
    assert_eq!(COUNTED.load(Ordering::SeqCst), 64, "each key ran once, whichever thread asked first");
}

const TEXT: SchemaHash = SchemaHash(ContentId([0x54; 32]));

blaze_format::record! {
    #[derive(Clone, Copy)]
    struct TextRoot: 1 {
        text: Slice<u8>,
    }
}

/// Bytes read in place: a value whose [`Persist::Ref`] borrows the database.
struct Text(String);

impl Persist for Text {
    const SCHEMA: SchemaHash = TEXT;
    type Ref<'a> = &'a [u8];
    fn write(&self, out: &mut Writer) -> u64 {
        let at = out.push_str(&self.0);
        let root = out.next::<TextRoot>();
        out.push(&TextRoot { text: Slice::between(root, at, self.0.len()) })
    }
    fn read(buffer: &[u8]) -> Result<&[u8], FormatError> {
        blaze_format::open_as::<TextRoot>(buffer, TEXT)?.text.follow(buffer)
    }
}

/// A file's bytes.
struct Contents;

impl Query for Contents {
    const KIND: QueryKind = QueryKind(0x2f0b);
    const NAME: &'static str = "contents";
    const PRODUCER: ContentId = ContentId([13; 32]);
    type Key = String;
    type Value = Text;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Text, Cancelled> {
        ran("contents");
        let file = cx.file(key)?.expect("a file");
        Ok(Text(String::from_utf8(file.bytes.to_vec()).expect("text")))
    }
}

/// Eviction swaps the value out; the buffer a task read stays until the task ends, and is
/// freed once every task that could hold it has.
#[test]
fn a_value_held_by_a_task_outlives_its_eviction() {
    use std::time::{Duration, Instant};

    let (mut db, tree) = setup();
    db.register::<Contents>();
    let one = view(&db, &tree, &[("a", "the bytes of a")]);
    let pinned = db.pin();
    let held = pinned.get::<Contents>(one, &"a".to_owned()).expect("contents");
    assert_eq!(db.evict(&key::<Contents>("a")), 1);
    assert_eq!(db.memo(&key::<Contents>("a")).map(|memo| memo.value), Some(Stored::Evicted));
    assert_eq!(db.held(), 1, "the evicted buffer waits for this task");
    assert_eq!(held, b"the bytes of a");
    drop(pinned);

    assert_eq!(db.pin().get::<Contents>(one, &"a".to_owned()), Ok(&b"the bytes of a"[..]));
    assert_eq!(runs("contents"), 2, "made again");
    let until = Instant::now() + Duration::from_secs(60);
    while db.held() > 0 {
        assert!(Instant::now() < until, "the evicted buffer was never freed");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn edges_round_trip_through_their_record() {
    let edge = Edge {
        read: QueryKey { kind: QueryKind(0x1234), input: ContentId([0xab; 32]) },
        saw: Fingerprint(0x0123_4567_89ab_cdef_0011_2233_4455_6677),
    };
    let record = EdgeRecord::new(edge.read, edge.saw);
    assert_eq!(record.edge(), edge);
    let bytes = crate::image::encode_edge(&edge);
    assert_eq!(bytes.len(), crate::table::EDGE_RECORD);
    assert_eq!(EdgeRecord::as_bytes(&[record]), bytes.as_slice(), "the image's layout, read in place");
    let aligned = Aligned::copy(&bytes);
    // SAFETY: `Aligned` is at a 16-byte boundary.
    let read = unsafe { EdgeRecord::from_bytes(aligned.bytes()) };
    assert_eq!(read.iter().map(EdgeRecord::edge).collect::<Vec<_>>(), vec![edge]);
}

/// Counts each thread's allocations for the budgets below and in `memo` and `table`'s tests.
#[global_allocator]
static ALLOCATOR: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

static WARM_CX: AtomicU64 = AtomicU64::new(u64::MAX);

/// [`Double`] asked twice: the second time is a warm hit inside a query, its allocations
/// counted into [`WARM_CX`].
struct AskTwice;

impl Query for AskTwice {
    const KIND: QueryKind = QueryKind(0x2f0c);
    const NAME: &'static str = "ask twice";
    const PRODUCER: ContentId = ContentId([14; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        let first = cx.get::<Double>(key)?;
        let (second, used) = blaze_alloc_count::measure(|| cx.get::<Double>(key));
        WARM_CX.store(used.allocations, Ordering::SeqCst);
        Ok(Num(first + second?))
    }
}

/// A hit on a key the view resolved, from outside a query and from inside one, allocates
/// nothing: no key bytes, no run, no edge buffer, no clone of the value.
#[test]
fn a_warm_hit_allocates_nothing() {
    let (mut db, tree) = setup();
    db.register::<AskTwice>();
    let one = view(&db, &tree, &[("a", "abc")]);
    let name = "a".to_owned();
    let pinned = db.pin();
    assert_eq!(pinned.get::<Double>(one, &name), Ok(6));
    let got = blaze_alloc_count::assert_at_most(0, "a warm Pinned::get", || pinned.get::<Double>(one, &name));
    assert_eq!(got, Ok(6));
    drop(pinned);

    assert_eq!(db.pin().get::<AskTwice>(one, &name), Ok(12));
    assert_eq!(WARM_CX.load(Ordering::SeqCst), 0, "a warm Cx::get allocated");
}

/// The key bytes of a key asked once stay beside its memos, so another view can verify an
/// edge on it with no caller naming it again; a key asked again is not encoded again.
#[test]
fn a_key_is_kept_once_and_runs_again_by_its_bytes() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let task = blaze_reclaim::enter();
    let slot = db.slot(&task, &key::<Len>("a")).expect("len(a) ran");
    let kept = slot.key().expect("its key's bytes").as_ptr();
    drop(task);
    double(&db, one, "a");
    let task = blaze_reclaim::enter();
    let again = db.slot(&task, &key::<Len>("a")).and_then(|slot| slot.key()).expect("still kept");
    assert_eq!(again.as_ptr(), kept, "not encoded and swapped in again");
    drop(task);

    // Another view: double(a) verifies its edge on len(a), whose memo no longer holds, so
    // len runs again from its kept bytes alone.
    let longer = view(&db, &tree, &[("a", "abcd")]);
    assert_eq!(db.pin().get::<Double>(longer, &"a".to_owned()), Ok(8));
    assert_eq!(runs("len"), 2);
}

/// Collection's marks are a bit a memo: an ID past the end they were made for grows them, and
/// they list what was marked in order.
#[test]
fn marks_are_a_bit_a_memo() {
    let mut marks = Marks::new(70);
    assert!(marks.insert(3));
    assert!(!marks.insert(3), "marked once");
    assert!(marks.insert(64));
    assert!(marks.insert(200), "past the end it was made for");
    assert!(marks.contains(64) && marks.contains(200) && !marks.contains(65) && !marks.contains(1000));
    assert_eq!(marks.iter().collect::<Vec<_>>(), [3, 64, 200]);
}

const BROKEN: Corrupt = Corrupt { section: SectionId::VFS_BLOBS, group: 7, cause: Cause::Crc { found: 1, wanted: 2 } };

/// A [`MemTree`] whose file `broken` names fails its check while it is set.
struct Breaking {
    files: MemTree,
    broken: Mutex<Option<&'static str>>,
}

impl Tree for Breaking {
    fn file(&self, root: ContentId, path: &str) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt> {
        if *self.broken.lock().unwrap() == Some(path) {
            return Err(BROKEN);
        }
        self.files.file(root, path)
    }
    fn list(&self, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
        self.files.list(root, path)
    }
}

#[test]
fn a_file_that_fails_its_check_fails_the_request_and_is_read_again() {
    reset();
    let tree = Arc::new(Breaking { files: MemTree::default(), broken: Mutex::new(Some("a")) });
    let mut db = Database::new(Config { tree: Some(tree.clone()), dir: FileName::new("db"), tags: TAGS });
    register(&mut db);
    let view = db.open(View { root: tree.files.root(&[("a", b"four".as_slice()), ("b", b"xy".as_slice())]) });
    assert_eq!(db.pin().get::<Double>(view, &"a".to_owned()).err(), Some(DbError::Corrupt(BROKEN)));
    assert_eq!(double(&db, view, "b"), 4, "another file of the view still reads");
    *tree.broken.lock().unwrap() = None;
    assert_eq!(double(&db, view, "a"), 8, "the failed read was not kept");
    assert_eq!(runs("len"), 3);
}
