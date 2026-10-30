//! blaze v2's query database (docs/v2/plan/lanes/db.md section 7, docs/v2/plan/lanes/items.md section 6.2), remade
//! from `vendor/salsa`.
//!
//! - A query is asked of a [`QueryKey`]: its kind and the content ID of its arguments. Results
//!   are keyed by input hashes, never by view or revision, so every view shares every equal
//!   result, and equal work in flight runs once (the plane's single flight, keyed by
//!   [`QueryKey::id`]).
//! - Each result keeps a [`Fingerprint`], and each dependency an [`Edge`]: what was read and
//!   the fingerprint it had then. A rerun that produces the same fingerprint stops there
//!   (early cutoff); fingerprints outlive evicted values, so eviction never breaks cutoff.
//!   No result is reused unless every edge's fingerprint is equal (#350).
//! - A **fact** is the narrowest thing a query read (docs/v2/plan/lanes/items.md section 6.2's table): a
//!   query of its own whose value is small and whose fingerprint is the hash it stands for (an
//!   interface hash, a layout hash, "N in S resolves to X"). The kinds of fact are
//!   `blaze-items`'s.
//! - The only per-view queries are the inputs ([`Input`]): a file of the view, the view's
//!   configuration. Everything else is a function of content.
//! - [`Why`] answers, for a query that ran, the chain of edges back to the input that changed.
//!
//! How it works (docs/v2/plan/lanes/db/query-database.md):
//!
//! - A key keeps every memo made of it, each with its edges in the order they were read
//!   (constructive traces). In a view, a memo is the key's result when each of its edges, in
//!   order, has the fingerprint it saw; verifying stops at the first that differs, so it never
//!   asks a dependency the query would not ask (a key read after a changed one may not exist
//!   any more). A query is deterministic, so at most one memo of a key holds in a view.
//! - A view is a root, which never changes: what a view resolved stays resolved, with no
//!   revision. Another view verifies the same memos against its own inputs, sharing every memo
//!   whose inputs are equal.
//! - A memo is a `u32` row of the store's columns (`memo`); its edges are one range of the
//!   edge segment of the thread that ran it, its value bytes in that thread's value segment
//!   (`segment`). A value is in memory, in a mapped file (a cold pack or the image), or
//!   evicted, its fingerprint kept; an evicted value asked for is made again and must have the
//!   fingerprint.
//! - A lookup takes no lock and writes no shared memory (docs/v2/plan/design/concurrency.md):
//!   every table is open-addressed over rows that never move, published with one release
//!   store, and a memo's value is one word (its row and that row's generation), swapped with a
//!   compare-and-swap to evict it, move it to disk or refill it. A warm hit allocates nothing.
//! - What a writer unlinks (a row, a table, a value, a memo, a closed view) is freed once every
//!   task that could have read it has ended ([`blaze_reclaim`]). Everything the database hands
//!   out borrows the task it was read in: a query's [`Cx`], or a [`Pinned`] from outside one.

mod image;
mod memo;
mod segment;
mod table;
pub mod tree;

#[cfg(test)]
mod tests;

use std::{
    any::{Any, TypeId},
    cell::{Cell, RefCell},
    fmt,
    sync::{Arc, OnceLock, atomic::AtomicU64, atomic::Ordering},
};

use blaze_core::{Completion, Done, Effect, FileName, Mapping, Tag};
use blaze_format::{Persist, SchemaHash, Writer};
use blaze_graph::pump::{self, Flights};
use blaze_ids::{ContentId, Fingerprint, Hasher, Idx};
use blaze_protocol::cbor;
use blaze_reclaim::{Grace, Guard, Limbo};
use blaze_vfs::{View, ViewId};
use serde::{Deserialize, Serialize};

use crate::{
    image::ImageMap,
    memo::{MemoId, Slot, Store},
    segment::Span,
    table::{Column, EdgeRecord, Edit, FxMap, FxSet, Map, ShortLock},
};
pub use crate::tree::{MemTree, Tree};

/// A kind of query, by number. Numbers are given in `blaze-query`'s [`kinds`] for the
/// database's own, and by the crate that defines the rest (facts are `blaze-items`'s); one
/// number is one query for every blaze that reads a given schema.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[serde(transparent)]
pub struct QueryKind(pub u16);

/// The database's own kinds, and the ranges given to others.
pub mod kinds {
    use super::QueryKind;

    /// A file of a view, by path: its content ID.
    pub const FILE: QueryKind = QueryKind(1);
    /// A directory of a view, by path: the content ID of its sorted names.
    pub const DIR: QueryKind = QueryKind(2);
    /// The view's configuration (target, profile, features asked for): its content ID.
    pub const CONFIG: QueryKind = QueryKind(3);
    /// Facts (`blaze-items`): `0x1000..0x2000`.
    pub const FACTS: std::ops::Range<u16> = 0x1000..0x2000;
    /// The front end's queries (parse, expand, resolve, infer, MIR): `0x2000..0x4000`.
    pub const FRONT: std::ops::Range<u16> = 0x2000..0x4000;
    /// Codegen and link: `0x4000..0x5000`.
    pub const BACK: std::ops::Range<u16> = 0x4000..0x5000;
    /// Build scripts, proc macros and tests: `0x5000..0x6000`.
    pub const USER: std::ops::Range<u16> = 0x5000..0x6000;
}

fn is_input(kind: QueryKind) -> bool {
    matches!(kind, kinds::FILE | kinds::DIR | kinds::CONFIG)
}

/// What a query is asked of: its kind and the content ID of its arguments, which are content
/// IDs themselves (never a dense index, a view or a revision).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct QueryKey {
    pub kind: QueryKind,
    pub input: ContentId,
}

impl QueryKey {
    pub const DOMAIN: &'static str = "blaze v2 query key 1";

    /// The key's own content ID: the plane's task key and the store's key of its result.
    pub fn id(&self) -> ContentId {
        blaze_ids::Hasher::new(QueryKey::DOMAIN)
            .update(&self.kind.0.to_le_bytes())
            .id(&self.input)
            .finish()
    }

    pub fn task_key(&self) -> blaze_graph::Key {
        blaze_graph::Key(self.id().0)
    }
}

/// One dependency of a result: the query it read, and that query's fingerprint when it did.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Edge {
    pub read: QueryKey,
    pub saw: Fingerprint,
}

/// What the database keeps of one result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Memo {
    pub key: QueryKey,
    pub fingerprint: Fingerprint,
    /// In the order they were read: verifying walks them in this order and stops at the first
    /// that differs.
    pub edges: Vec<Edge>,
    /// The schema of the stored value and the content ID of the code that produced it (#1439):
    /// a value of another schema or producer is not this one.
    pub schema: SchemaHash,
    pub producer: ContentId,
    /// Where the value is: in memory, on disk in the store, or gone (its fingerprint kept).
    pub value: Stored,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stored {
    Memory,
    /// In the store, under this key.
    Disk(ContentId),
    Evicted,
}

/// What a query reads of a view directly.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Input {
    File(String),
    Dir(String),
    Config,
}

/// The domain of an input's path in its [`QueryKey`].
pub const PATH_DOMAIN: &str = "blaze v2 db input path 1";
pub const CONFIG_DOMAIN: &str = "blaze v2 db input config 1";
/// The domain of a value's bytes: its content ID, and the first 128 bits of it its default
/// fingerprint.
pub const VALUE_DOMAIN: &str = "blaze v2 db value 1";
const ABSENT_DOMAIN: &str = "blaze v2 db input absent 1";
const NAMES_DOMAIN: &str = "blaze v2 db input names 1";
const FILE_DOMAIN: &str = "blaze v2 db input file 1";

impl Input {
    pub fn key(&self) -> QueryKey {
        match self {
            Input::File(path) => Input::file_key(path),
            Input::Dir(path) => Input::dir_key(path),
            Input::Config => Input::config_key(),
        }
    }

    /// [`Input::File`]'s key, made without the input.
    pub fn file_key(path: &str) -> QueryKey {
        QueryKey { kind: kinds::FILE, input: ContentId::of(PATH_DOMAIN, path.as_bytes()) }
    }

    /// [`Input::Dir`]'s key, made without the input.
    pub fn dir_key(path: &str) -> QueryKey {
        QueryKey { kind: kinds::DIR, input: ContentId::of(PATH_DOMAIN, path.as_bytes()) }
    }

    pub fn config_key() -> QueryKey {
        static KEY: OnceLock<QueryKey> = OnceLock::new();
        *KEY.get_or_init(|| QueryKey { kind: kinds::CONFIG, input: ContentId::of(CONFIG_DOMAIN, b"") })
    }
}

/// A query: a pure function of its key, read through [`Cx`], which records every edge.
pub trait Query: 'static {
    const KIND: QueryKind;
    const NAME: &'static str;
    /// The content ID of the code of the query's function, for [`Memo::producer`]; set by the
    /// build (blaze's own item hashes of it).
    const PRODUCER: ContentId;
    /// Kept as bytes (deterministic CBOR) beside the key's memos and in the image, so a
    /// dependency no one names again can still be run to verify an edge on it.
    type Key: Clone + Send + Sync + Serialize + serde::de::DeserializeOwned + 'static;
    type Value: Persist;
    /// The content ID of `key`, for [`QueryKey::input`].
    fn input(key: &Self::Key) -> ContentId;
    fn execute(cx: &mut Cx<'_>, key: &Self::Key) -> Result<Self::Value, Cancelled>;
    /// The value's fingerprint, given the value and its bytes in the format (written once,
    /// as the database keeps them): by default the hash of the bytes.
    fn fingerprint(_value: &Self::Value, bytes: &[u8]) -> Fingerprint {
        value_fingerprint(bytes)
    }
}

/// `value` written into a buffer of its own, as the database keeps it.
pub fn write_value<V: Persist>(value: &V) -> Vec<u8> {
    let mut out = Writer::new(V::SCHEMA);
    let root = value.write(&mut out);
    out.finish(root)
}

/// The fingerprint of a value's bytes.
pub fn value_fingerprint(bytes: &[u8]) -> Fingerprint {
    value_id(bytes).fingerprint()
}

pub(crate) fn value_id(bytes: &[u8]) -> ContentId {
    ContentId::of(VALUE_DOMAIN, bytes)
}

/// A key handed to a query's runner: the caller's own, or the bytes the database kept.
#[derive(Clone, Copy)]
enum KeySource<'a> {
    Typed(&'a (dyn Any + Send + Sync)),
    Bytes(&'a [u8]),
}

/// A run's value, written once; copied once into the thread's value segment by the store.
struct Computed {
    bytes: Vec<u8>,
    fingerprint: Fingerprint,
}

/// Runs one kind's query: `None` when its key's bytes don't decode (a key of another schema,
/// from an image), which makes the memo that named it unverifiable, never wrong.
type Runner = fn(&mut Cx<'_>, KeySource<'_>) -> Result<Option<Computed>, Cancelled>;

fn run_query<Q: Query>(cx: &mut Cx<'_>, source: KeySource<'_>) -> Result<Option<Computed>, Cancelled> {
    let decoded: Q::Key;
    let key = match source {
        KeySource::Typed(key) => key
            .downcast_ref::<Q::Key>()
            .unwrap_or_else(|| panic!("query {} was handed a key of another type", Q::NAME)),
        KeySource::Bytes(bytes) => match cbor::from_slice::<Q::Key>(bytes) {
            Ok(key) => {
                decoded = key;
                &decoded
            }
            Err(_) => return Ok(None),
        },
    };
    let value = Q::execute(cx, key)?;
    let bytes = write_value(&value);
    let fingerprint = Q::fingerprint(&value, &bytes);
    Ok(Some(Computed { bytes, fingerprint }))
}

struct KindInfo {
    name: &'static str,
    producer: ContentId,
    schema: SchemaHash,
    /// The store's number of `(schema, producer)`: a memo of another is never reused (#1439).
    code: u32,
    query: TypeId,
    run: Runner,
}

/// This thread's runs: the keys they are running, outermost first, and the edges each
/// running query has read so far, a query's above those of the query that asked it. Runs
/// nest as calls do (a query asks another inline, a wait pumps a job above it), so each takes
/// the top of both stacks and gives it back when it returns; the buffers stay, so a run
/// allocates nothing once they have grown.
struct Stacks {
    keys: Vec<QueryKey>,
    edges: Vec<EdgeRecord>,
}

thread_local! {
    /// The pump depth of the plane task that runs a query on this thread, if one does. A job
    /// the thread pumps while that query waits is a task above it, free to ask the database.
    static INSIDE: Cell<Option<usize>> = const { Cell::new(None) };
    static STACKS: RefCell<Stacks> = const { RefCell::new(Stacks { keys: Vec::new(), edges: Vec::new() }) };
}

fn inside_a_query() -> bool {
    INSIDE.with(Cell::get) == Some(pump::depth())
}

/// Marks this thread's task as running a query while it lives.
struct Inside(Option<usize>);

impl Inside {
    fn enter() -> Inside {
        Inside(INSIDE.with(|it| it.replace(Some(pump::depth()))))
    }
}

impl Drop for Inside {
    fn drop(&mut self) {
        INSIDE.with(|it| it.set(self.0));
    }
}

/// One request's root and everything it runs inline, on one task: the keys of this thread's
/// stack from `base` on. A job pumped above a waiting query is a run of its own.
#[derive(Clone, Copy)]
struct Run {
    base: usize,
}

impl Run {
    fn new() -> Run {
        Run { base: STACKS.with(|stacks| stacks.borrow().keys.len()) }
    }

    /// Panics, naming the chain, if `key` is running in this run already.
    fn check(self, key: QueryKey) {
        STACKS.with(|stacks| {
            let stacks = stacks.borrow();
            let running = &stacks.keys[self.base.min(stacks.keys.len())..];
            if running.contains(&key) {
                panic!(
                    "a query cycle: {} asks {key:?} while running it",
                    running.iter().map(|it| format!("{it:?}")).collect::<Vec<_>>().join(" -> ")
                );
            }
        });
    }
}

/// A key on this thread's stack while it lives.
struct Frame;

impl Frame {
    fn push(key: QueryKey) -> Frame {
        STACKS.with(|stacks| stacks.borrow_mut().keys.push(key));
        Frame
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        _ = STACKS.try_with(|stacks| {
            if let Ok(mut stacks) = stacks.try_borrow_mut() {
                stacks.keys.pop();
            }
        });
    }
}

/// The edges one execution read: this thread's edge stack from `base` on, given back (cut to
/// `base`) when it drops, whether the run made a memo, failed or unwound.
struct Recording {
    base: usize,
}

impl Recording {
    fn start() -> Recording {
        Recording { base: STACKS.with(|stacks| stacks.borrow().edges.len()) }
    }

    fn with<R>(&self, f: impl FnOnce(&[EdgeRecord]) -> R) -> R {
        STACKS.with(|stacks| f(&stacks.borrow().edges[self.base..]))
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        _ = STACKS.try_with(|stacks| {
            if let Ok(mut stacks) = stacks.try_borrow_mut() {
                stacks.edges.truncate(self.base);
            }
        });
    }
}

/// A task's view of the database while it runs a query: everything read through it is an
/// edge of the result, pushed onto this thread's edge stack and copied once, with the result,
/// into its edge segment.
///
/// What it hands out lives as long as `'db`: the task the query runs in, which keeps whatever
/// it read from being freed.
pub struct Cx<'db> {
    db: &'db Database,
    task: &'db Guard,
    state: &'db ViewState,
    run: Run,
}

impl<'db> Cx<'db> {
    pub fn view(&self) -> ViewId {
        self.state.id
    }

    pub fn database(&self) -> &'db Database {
        self.db
    }

    fn record(&mut self, read: QueryKey, saw: Fingerprint) {
        STACKS.with(|stacks| stacks.borrow_mut().edges.push(EdgeRecord::new(read, saw)));
    }

    /// Another query's value: reused, waited on if running, or run inline. Records the edge.
    pub fn get<Q: Query>(
        &mut self,
        key: &Q::Key,
    ) -> Result<<Q::Value as Persist>::Ref<'db>, Cancelled> {
        let (db, task, state) = (self.db, self.task, self.state);
        if db.kind_of::<Q>().is_none() {
            panic!("query {} (kind {}) is asked but was never registered", Q::NAME, Q::KIND.0);
        }
        let query = QueryKey { kind: Q::KIND, input: Q::input(key) };
        let memo = db.find::<Q>(task, state, query, self.run, key)?;
        self.record(query, db.store.fingerprint(task, memo));
        let bytes = db.value_of(task, state, query, memo, self.run, key)?;
        Ok(read_value::<Q>(bytes))
    }

    /// A file of the view: its content ID, mode and bytes; `None` for none. Records the edge.
    pub fn file(&mut self, path: &str) -> Result<Option<blaze_vfs::File<'db>>, Cancelled> {
        let key = Input::file_key(path);
        let value = self.db.input(self.task, self.state, key, || Input::File(path.to_owned()));
        self.record(key, value.fingerprint);
        Ok(match &value.data {
            InputData::File(Some((id, mode, bytes))) => {
                Some(blaze_vfs::File { id: *id, mode: *mode, bytes: &**bytes })
            }
            _ => None,
        })
    }

    /// The names in a directory of the view, sorted, each with whether it is a directory;
    /// `None` for none or a file. Records the edge.
    pub fn dir(&mut self, path: &str) -> Result<Option<&'db [(String, bool)]>, Cancelled> {
        let key = Input::dir_key(path);
        let value = self.db.input(self.task, self.state, key, || Input::Dir(path.to_owned()));
        self.record(key, value.fingerprint);
        Ok(match &value.data {
            InputData::Dir(Some(names)) => Some(&**names),
            _ => None,
        })
    }

    /// The view's configuration, as it was opened with ([`Database::open_with`]). Records the
    /// edge.
    pub fn config(&mut self) -> Result<&'db [u8], Cancelled> {
        let state = self.state;
        let key = Input::config_key();
        let value = self.db.input(self.task, state, key, || Input::Config);
        self.record(key, value.fingerprint);
        Ok(&*state.config)
    }

    /// Records that the query read `fact`, whose fingerprint was `saw`: for facts computed
    /// without asking a query (an index the caller holds).
    pub fn read(&mut self, fact: QueryKey, saw: Fingerprint) {
        self.record(fact, saw);
    }
}

fn read_value<'db, Q: Query>(bytes: &'db [u8]) -> <Q::Value as Persist>::Ref<'db> {
    Q::Value::read(bytes)
        .unwrap_or_else(|error| panic!("the kept value of query {} does not read: {error}", Q::NAME))
}

/// The query was cancelled: a nested request needs what an outer one holds (#1301), or its
/// request ended. It is reposted, never answered with a partial value. The plane's own, as a
/// query's wait is one of the plane's waits.
pub use blaze_graph::Cancelled;

/// Why a query reran: from it, each edge whose fingerprint differed, back to an input.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Why {
    pub query: QueryKey,
    /// The first is what `query` read that changed, the last an input.
    pub chain: Vec<Changed>,
    /// The input at the end, as the view names it.
    pub input: Option<Input>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Changed {
    pub key: QueryKey,
    pub was: Fingerprint,
    pub now: Fingerprint,
}

/// Why the database could not answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DbError {
    Cancelled,
    /// The view is not open.
    NoView(ViewId),
    /// A query kind asked that no one registered.
    Unknown(QueryKind),
    /// The image is of another schema: it is rebuilt from the store's results.
    Image(blaze_format::FormatError),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Cancelled => write!(f, "cancelled"),
            DbError::NoView(view) => write!(f, "view {view:?} is not open"),
            DbError::Unknown(kind) => write!(f, "no query of kind {} is registered", kind.0),
            DbError::Image(error) => write!(f, "the daemon image: {error}"),
        }
    }
}

/// How a database reads and where it keeps what it writes.
#[derive(Clone)]
pub struct Config {
    /// The files of every view; `None` for a database no query of which reads a file.
    pub tree: Option<Arc<dyn Tree>>,
    /// Its directory under the driver's root: `<dir>/image` and `<dir>/cold/<n>`. `None` keeps
    /// every value in memory and writes no image.
    pub dir: Option<FileName>,
    /// Tags it asks with are `tags..tags + 2^32`.
    pub tags: u64,
}

impl Config {
    /// No tree and no directory: everything in memory.
    pub fn memory() -> Config {
        Config { tree: None, dir: None, tags: 0 }
    }
}

/// What a view resolved: every key's memo, every input it read, and why each key it ran
/// again did.
///
/// A result is 40 bytes of row (the key and the memo's ID) and at most 16 of places; finding
/// one is a shard's table load, about 1.5 place loads and one row read, with no hash computed.
struct ViewState {
    id: ViewId,
    view: View,
    config: Arc<[u8]>,
    results: Map<MemoId>,
    inputs: Map<InputValue>,
    reruns: Map<Rerun>,
}

impl ViewState {
    fn new(id: ViewId, view: View, config: Arc<[u8]>) -> ViewState {
        ViewState { id, view, config, results: Map::new(), inputs: Map::new(), reruns: Map::new() }
    }

    fn collect(&self) {
        self.results.collect();
        self.inputs.collect();
        self.reruns.collect();
    }

    fn reclaim(&mut self) {
        self.results.reclaim();
        self.inputs.reclaim();
        self.reruns.reclaim();
    }
}

/// Why a key ran in a view: the newest memo it had then (its fingerprint) and that memo's
/// first edge that differed, `None` where the edge's query could not be run by key; no memo
/// at all for a first run.
#[derive(Clone)]
struct Rerun {
    previous: Option<(Fingerprint, Option<Changed>)>,
}

#[derive(Clone)]
struct InputValue {
    fingerprint: Fingerprint,
    data: InputData,
}

#[derive(Clone)]
enum InputData {
    File(Option<(ContentId, blaze_vfs::Mode, Arc<[u8]>)>),
    Dir(Option<Arc<[(String, bool)]>>),
    Config,
}

enum Verdict {
    Valid,
    Changed(Changed),
    Unknown,
}

/// A value a cold pack holds: its memo, pinned in the store until the pack is mapped, and the
/// word that named the value when it was copied, which [`Store::to_disk`] swaps only if the
/// memo's word still is.
struct ColdEntry {
    memo: MemoId,
    word: u64,
    at: usize,
    len: usize,
    id: ContentId,
}

enum Pending {
    ColdWrite { file: FileName, entries: Vec<ColdEntry> },
    ColdMap { file: FileName, entries: Vec<ColdEntry> },
    ImageWrite { file: FileName },
    ImageMap { file: FileName },
    Remove { file: FileName },
}

impl Pending {
    fn what(&self) -> String {
        match self {
            Pending::ColdWrite { file, .. } => format!("write its cold pack {file}"),
            Pending::ColdMap { file, .. } => format!("map its cold pack {file}"),
            Pending::ImageWrite { file } => format!("write its image {file}"),
            Pending::ImageMap { file } => format!("map its image {file}"),
            Pending::Remove { file } => format!("remove {file}"),
        }
    }
}

#[derive(Default)]
struct Io {
    next: u64,
    pending: FxMap<Tag, Pending>,
    /// Cold packs mapped, kept while values are read from them: as long as the database.
    packs: Vec<(FileName, Mapping)>,
    cold_files: u64,
    /// The image written and mapped, for [`Database::settle`].
    settled: Option<Mapping>,
}

impl Io {
    fn tag(&mut self, base: u64) -> Tag {
        let tag = Tag(base + (self.next & 0xffff_ffff));
        self.next += 1;
        tag
    }
}

/// The one database of a daemon: every view's results, the memo of each key.
///
/// Fields drop in order: the tables first (a slot holds spans of the store's key segments),
/// then the store, then the mappings its rows name.
pub struct Database {
    config: Config,
    kinds: FxMap<QueryKind, KindInfo>,
    /// Every key's memos made or touched since start; the rest are in the image.
    slots: Map<Slot>,
    /// What each input key a memo read names, to read it again in another view.
    inputs: Map<Input>,
    /// By [`ViewId`]; a closed one is taken out.
    views: Column<ViewState>,
    /// One run of a key at a time, whatever view asked, on the plane's wait graph (#1301).
    flights: Flights<()>,
    /// Closed views, until every task that could hold them has ended.
    limbo: ShortLock<Limbo>,
    /// Bumped by each collection; a key given a memo since one started is kept by it whole.
    generation: AtomicU64,
    /// The generation each key was last given a memo in.
    born: Map<u64>,
    store: Store,
    image: Option<ImageMap>,
    io: ShortLock<Io>,
}

/// The database read from outside any query: a request's root. What it hands out borrows it,
/// and it keeps this thread in a task ([`blaze_reclaim::enter`]) so nothing read is freed
/// under it. Hold it for one request: what the database lets go of meanwhile is freed only
/// once it is dropped.
pub struct Pinned<'db> {
    db: &'db Database,
    task: Guard,
}

impl Pinned<'_> {
    /// A query's value in `view`.
    pub fn get<Q: Query>(
        &self,
        view: ViewId,
        key: &Q::Key,
    ) -> Result<<Q::Value as Persist>::Ref<'_>, DbError> {
        let (db, task) = (self.db, &self.task);
        assert!(
            !inside_a_query(),
            "Pinned::get of {} from inside a query: ask it through Cx::get, which records it",
            Q::NAME
        );
        if db.kind_of::<Q>().is_none() {
            return Err(DbError::Unknown(Q::KIND));
        }
        let state = db.view_state(task, view).ok_or(DbError::NoView(view))?;
        let query = QueryKey { kind: Q::KIND, input: Q::input(key) };
        let run = Run::new();
        let memo = db.find::<Q>(task, state, query, run, key).map_err(|Cancelled| DbError::Cancelled)?;
        let bytes =
            db.value_of(task, state, query, memo, run, key).map_err(|Cancelled| DbError::Cancelled)?;
        Ok(read_value::<Q>(bytes))
    }
}

impl Database {
    fn blank(config: Config) -> Database {
        Database {
            config,
            kinds: FxMap::default(),
            slots: Map::new(),
            inputs: Map::new(),
            views: Column::new(),
            flights: Flights::new(),
            limbo: ShortLock::default(),
            generation: AtomicU64::new(0),
            born: Map::new(),
            store: Store::new(),
            image: None,
            io: ShortLock::default(),
        }
    }

    /// A database with nothing in it, in memory, that reads no files: for queries of content
    /// only and tests. A daemon's is [`Database::new`].
    pub fn empty() -> Database {
        Database::blank(Config::memory())
    }

    pub fn new(config: Config) -> Database {
        Database::blank(config)
    }

    /// The database in a mapped image, used in place.
    pub fn from_image(image: Mapping) -> Result<Database, DbError> {
        Database::from_image_with(Config::memory(), image)
    }

    /// The database in a mapped image, used in place, reading and writing as `config` says.
    pub fn from_image_with(config: Config, image: Mapping) -> Result<Database, DbError> {
        let image = ImageMap::open(image).map_err(DbError::Image)?;
        let db = Database::blank(config);
        {
            let task = blaze_reclaim::enter();
            for (key, input) in image.inputs() {
                db.inputs.insert(&task, key, input);
            }
        }
        Ok(Database { image: Some(image), ..db })
    }

    pub fn register<Q: Query>(&mut self) {
        assert!(!is_input(Q::KIND), "query {} takes kind {}, an input's", Q::NAME, Q::KIND.0);
        if let Some(info) = self.kinds.get(&Q::KIND) {
            assert!(
                info.query == TypeId::of::<Q>(),
                "queries {} and {} both take kind {}",
                info.name,
                Q::NAME,
                Q::KIND.0
            );
            return;
        }
        self.kinds.insert(
            Q::KIND,
            KindInfo {
                name: Q::NAME,
                producer: Q::PRODUCER,
                schema: Q::Value::SCHEMA,
                code: self.store.code(Q::Value::SCHEMA, Q::PRODUCER),
                query: TypeId::of::<Q>(),
                run: run_query::<Q>,
            },
        );
    }

    pub fn open(&self, view: View) -> ViewId {
        self.open_with(view, &[])
    }

    /// Opens `view` with its configuration (target, profile, features asked for), what
    /// [`Cx::config`] reads.
    pub fn open_with(&self, view: View, config: &[u8]) -> ViewId {
        let config: Arc<[u8]> = Arc::from(config);
        Idx::from_raw(self.views.push(|index| ViewState::new(Idx::from_raw(index), view, config)))
    }

    pub fn close(&self, view: ViewId) {
        if let Some(state) = self.views.take(view.raw()) {
            self.retire([state]);
        }
    }

    /// The database for one request from outside any query: see [`Pinned`].
    pub fn pin(&self) -> Pinned<'_> {
        Pinned { db: self, task: blaze_reclaim::enter() }
    }

    pub fn memo(&self, key: &QueryKey) -> Option<Memo> {
        let task = blaze_reclaim::enter();
        let &memo = self.slot(&task, key)?.memos().last()?;
        let hot = self.store.hot(&task, memo);
        let (schema, producer) = self.store.parts(hot.code());
        Some(Memo {
            key: *key,
            fingerprint: hot.fingerprint(),
            edges: self.store.edges(&task, memo).iter().map(EdgeRecord::edge).collect(),
            schema,
            producer,
            value: self.store.stored(&task, memo),
        })
    }

    pub fn why(&self, view: ViewId, key: &QueryKey) -> Option<Why> {
        let task = blaze_reclaim::enter();
        let state = self.view_state(&task, view)?;
        let rerun = state.reruns.get(&task, key)?;
        let mut why = Why { query: *key, chain: Vec::new(), input: None };
        let Some((_, Some(mut changed))) = rerun.previous else { return Some(why) };
        let mut seen = FxSet::default();
        loop {
            why.chain.push(changed);
            if is_input(changed.key.kind) {
                why.input = self.inputs.get(&task, &changed.key).cloned();
                break;
            }
            if !seen.insert(changed.key) {
                break;
            }
            match self.why_changed(&task, state, changed) {
                Some(next) => changed = next,
                None => break,
            }
        }
        Some(why)
    }

    /// Drops what no open view and no key of `kept` reaches; moves cold values to the store.
    pub fn collect(&self, kept: &[QueryKey], effects: &mut Vec<Effect>) {
        let task = blaze_reclaim::enter();
        let started = self.generation.fetch_add(1, Ordering::SeqCst);
        // Before the views are walked: a memo unlinked earlier is checked against what they
        // hold now (see `memo`'s comment).
        let dying = self.store.to_check();
        let mut reached: FxSet<MemoId> = FxSet::default();
        let mut hot: FxSet<MemoId> = FxSet::default();
        let mut inputs_kept: FxSet<QueryKey> = FxSet::default();
        let mut work: Vec<MemoId> = Vec::new();
        self.views.for_each(&task, |state| {
            state.results.for_each(&task, |_, &memo| {
                hot.insert(memo);
                work.push(memo);
            });
            state.inputs.for_each(&task, |key, _| {
                inputs_kept.insert(*key);
            });
        });
        self.store.checked(dying, |memo| hot.contains(&memo));
        for key in kept {
            if is_input(key.kind) {
                inputs_kept.insert(*key);
            } else if let Some(slot) = self.slot(&task, key) {
                work.extend_from_slice(slot.memos());
            }
        }
        while let Some(memo) = work.pop() {
            if !reached.insert(memo) {
                continue;
            }
            for edge in self.store.edges(&task, memo) {
                let read = edge.read();
                if is_input(read.kind) {
                    inputs_kept.insert(read);
                    continue;
                }
                if let Some(slot) = self.slot(&task, &read) {
                    let saw = edge.saw();
                    work.extend(slot.memos().iter().copied().filter(|&it| self.store.fingerprint(&task, it) == saw));
                }
            }
        }

        let young = |key: &QueryKey| self.born.get(&task, key).is_some_and(|born| *born > started);
        let mut unlinked = Vec::new();
        self.slots.edit_all_with(
            &task,
            |key, slot| {
                let memos = slot.memos();
                if young(key) {
                    for &memo in memos {
                        for edge in self.store.edges(&task, memo) {
                            if is_input(edge.read().kind) {
                                inputs_kept.insert(edge.read());
                            }
                        }
                    }
                    return Edit::Keep;
                }
                let live: Vec<MemoId> = memos.iter().copied().filter(|memo| reached.contains(memo)).collect();
                if live.len() == memos.len() {
                    Edit::Keep
                } else if !live.is_empty() {
                    Edit::Replace(self.store.slot(slot.share_key(), &live))
                } else if self.image.as_ref().is_some_and(|image| image.find(key).is_some()) {
                    // Left empty, not removed, so the image's memos of the key stay dropped.
                    Edit::Replace(self.store.slot(slot.share_key(), &[]))
                } else {
                    Edit::Remove
                }
            },
            |old, new| {
                let still = new.map_or(&[][..], Slot::memos);
                unlinked.extend(old.memos().iter().copied().filter(|memo| !still.contains(memo)));
            },
        );
        self.store.unlinked(unlinked);
        if let Some(image) = &self.image {
            for key in image.keys() {
                self.slots.get_or_insert_with(&task, key, Slot::empty);
            }
        }
        self.born.retain(&task, |_, born| *born > started);
        self.inputs.retain(&task, |key, _| inputs_kept.contains(key));

        if self.config.dir.is_some() {
            let mut cold: Vec<(QueryKey, Fingerprint, MemoId)> = reached
                .iter()
                .copied()
                .filter(|memo| !hot.contains(memo) && self.store.stored(&task, *memo) == Stored::Memory)
                .map(|memo| (self.store.key(&task, memo), self.store.fingerprint(&task, memo), memo))
                .collect();
            cold.sort_unstable();
            self.write_cold(&task, cold.into_iter().map(|(_, _, memo)| memo), effects);
        }
        self.collect_retired(&task);
    }

    /// How many closed views and values let go of are still held, after freeing those past
    /// their grace.
    #[cfg(test)]
    fn held(&self) -> usize {
        let mut limbo = self.limbo.lock();
        limbo.collect();
        limbo.len() + self.store.held()
    }

    /// Frees what the database's tables and store let go of whose grace has passed: the
    /// tables' rows first, as a slot's row holds spans of the store's segments.
    fn collect_retired(&self, task: &Guard) {
        self.limbo.lock().collect();
        self.slots.collect();
        self.inputs.collect();
        self.born.collect();
        self.views.for_each(task, ViewState::collect);
        self.store.collect();
    }

    /// Writes the whole database as one image, through effects.
    pub fn write_image(&self, effects: &mut Vec<Effect>) {
        let dir = self.dir("write its image");
        let file = FileName::new(&format!("{dir}/image")).expect("a name under the database's directory");
        let task = blaze_reclaim::enter();
        let entries = self.image_entries(&task);
        let bytes = image::write(&task, &self.store, &entries, &self.image_inputs(&task));
        drop(entries);
        drop(task);
        let mut io = self.io.lock();
        let tag = io.tag(self.config.tags);
        effects.push(Effect::Write { file: file.clone(), bytes, tag });
        io.pending.insert(tag, Pending::ImageWrite { file });
    }

    pub fn complete(&self, completion: Completion, effects: &mut Vec<Effect>) {
        let mut io = self.io.lock();
        let Some(pending) = io.pending.remove(&completion.tag) else {
            panic!("the database did not ask for {:?}, which completed as {:?}", completion.tag, completion.done)
        };
        match (pending, completion.done) {
            (Pending::ColdWrite { file, entries }, Done::Written) => {
                let tag = io.tag(self.config.tags);
                effects.push(Effect::Map { file: file.clone(), tag });
                io.pending.insert(tag, Pending::ColdMap { file, entries });
            }
            (Pending::ColdMap { file, entries }, Done::Mapped(mapping)) => {
                drop(io);
                let task = blaze_reclaim::enter();
                for entry in entries {
                    let bytes = mapping.bytes().get(entry.at..entry.at + entry.len);
                    let Some(bytes) = bytes.filter(|bytes| value_id(bytes) == entry.id) else {
                        panic!(
                            "the database's cold pack {file} does not hold at byte {} the value it wrote",
                            entry.at
                        );
                    };
                    // SAFETY: `mapping` goes into `io.packs` below, which the database keeps
                    // while it lives (settle drops the packs only with the store).
                    unsafe { self.store.to_disk(&task, entry.memo, entry.word, bytes, entry.id) };
                    self.store.unpin(entry.memo);
                }
                self.io.lock().packs.push((file, mapping));
            }
            (Pending::ImageWrite { file }, Done::Written) => {
                let tag = io.tag(self.config.tags);
                effects.push(Effect::Map { file: file.clone(), tag });
                io.pending.insert(tag, Pending::ImageMap { file });
            }
            (Pending::ImageMap { .. }, Done::Mapped(mapping)) => io.settled = Some(mapping),
            (Pending::Remove { .. }, Done::Removed) => {}
            (pending, Done::Failed(error)) => {
                panic!("the database could not {}: {error}", pending.what())
            }
            (pending, done) => panic!("the database asked to {} and was told {done:?}", pending.what()),
        }
    }
}

impl Database {
    /// Whether a completion is of an effect this asked for.
    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.config.tags) < 1 << 32
    }

    /// Whether the image [`Database::write_image`] wrote is mapped, for [`Database::settle`].
    pub fn settled(&self) -> bool {
        self.io.lock().settled.is_some()
    }

    /// The settle swap (#1299): once the image is written and mapped, the database becomes the
    /// image's, used in place, and every arena of the cold build is dropped. Open views stay
    /// open with what they resolved; queries stay registered. `false` when no image is mapped
    /// yet.
    pub fn settle(&mut self, effects: &mut Vec<Effect>) -> Result<bool, DbError> {
        let Some(mapping) = self.io.get_mut().settled.take() else { return Ok(false) };
        let mut new = Database::from_image_with(self.config.clone(), mapping)?;
        new.kinds = std::mem::take(&mut self.kinds);
        for info in new.kinds.values_mut() {
            info.code = new.store.code(info.schema, info.producer);
        }
        let (next, views) = self.views.take_all();
        let task = blaze_reclaim::enter();
        let views = views
            .into_iter()
            .map(|old| {
                let fresh = ViewState::new(old.id, old.view, old.config.clone());
                old.inputs.for_each(&task, |key, value| {
                    fresh.inputs.insert(&task, *key, value.clone());
                });
                old.reruns.for_each(&task, |key, rerun| {
                    fresh.reruns.insert(&task, *key, rerun.clone());
                });
                old.results.for_each(&task, |key, &memo| {
                    let same = new.slot(&task, key).and_then(|slot| {
                        slot.memos().iter().copied().find(|&it| new.store.same(&task, it, &self.store, memo))
                    });
                    if let Some(same) = same {
                        fresh.results.insert(&task, *key, same);
                    }
                });
                Box::new(fresh)
            })
            .collect();
        drop(task);
        new.views.restore(next, views, |state| state.id.raw());
        let old_io = std::mem::take(self.io.get_mut());
        let new_io = new.io.get_mut();
        new_io.next = old_io.next;
        new_io.cold_files = old_io.cold_files;
        new_io.pending = old_io.pending;
        for (file, _) in &old_io.packs {
            let tag = new_io.tag(new.config.tags);
            effects.push(Effect::Remove { file: file.clone(), tag });
            new_io.pending.insert(tag, Pending::Remove { file: file.clone() });
        }
        *self = new;
        // The old store, whose rows named the packs' bytes, is gone with the old database.
        drop(old_io.packs);
        Ok(true)
    }

    /// Frees everything let go of, past its grace or not: with `&mut`, nothing reads it.
    pub fn reclaim(&mut self) {
        *self.limbo.get_mut() = Limbo::new();
        self.slots.reclaim();
        self.inputs.reclaim();
        self.born.reclaim();
        let mut held: FxSet<MemoId> = FxSet::default();
        {
            let task = blaze_reclaim::enter();
            self.views.for_each(&task, |state| {
                state.results.for_each(&task, |_, &memo| {
                    held.insert(memo);
                });
            });
        }
        self.views.values_mut().for_each(ViewState::reclaim);
        self.store.reclaim(|memo| held.contains(&memo));
    }

    /// Drops the values of `key`'s memos from memory, keeping their fingerprints and edges: a
    /// value already kept elsewhere (compiled code in the store, #806). Asked again, it is made
    /// again and must have its fingerprint. A reader still holding one keeps it until its task
    /// ends. Returns how many were dropped.
    pub fn evict(&self, key: &QueryKey) -> usize {
        let task = blaze_reclaim::enter();
        let Some(slot) = self.slot(&task, key) else { return 0 };
        slot.memos().iter().filter(|&&memo| self.store.evict(&task, memo)).count()
    }

    fn dir(&self, doing: &str) -> &FileName {
        self.config
            .dir
            .as_ref()
            .unwrap_or_else(|| panic!("the database has no directory to {doing} in: make it with Database::new"))
    }

    fn tree(&self) -> &dyn Tree {
        self.config.tree.as_deref().unwrap_or_else(|| {
            panic!("the database has no file tree to read a view's files from: make it with Database::new")
        })
    }

    /// Holds `things`, already unlinked, until every task that could hold them has ended, and
    /// frees what earlier ones held that has passed its grace.
    fn retire<T: Send + Sync + 'static>(&self, things: impl IntoIterator<Item = T>) {
        let mut things = things.into_iter().peekable();
        if things.peek().is_none() {
            return;
        }
        let grace = Grace::start();
        let mut limbo = self.limbo.lock();
        for thing in things {
            limbo.hold(grace, thing);
        }
        limbo.collect();
    }

    fn view_state<'g>(&'g self, task: &'g Guard, view: ViewId) -> Option<&'g ViewState> {
        self.views.get(task, view.raw())
    }

    /// `Q`'s kind, if registered; a panic if another query took its kind.
    fn kind_of<Q: Query>(&self) -> Option<&KindInfo> {
        let info = self.kinds.get(&Q::KIND)?;
        if info.query != TypeId::of::<Q>() {
            panic!("query {} is asked, but kind {} is {}'s", Q::NAME, Q::KIND.0, info.name);
        }
        Some(info)
    }

    /// `query`'s memo in a view: a hit is one lookup in the view's results. Otherwise the
    /// key's bytes are kept (once) and the memo resolved.
    fn find<Q: Query>(
        &self,
        task: &Guard,
        state: &ViewState,
        query: QueryKey,
        run: Run,
        key: &Q::Key,
    ) -> Result<MemoId, Cancelled> {
        if let Some(&memo) = state.results.get(task, &query) {
            return Ok(memo);
        }
        self.learn_key::<Q>(task, query, key);
        Ok(self
            .resolve(task, state, query, run, Some(key))?
            .expect("a registered query asked with its key always resolves"))
    }

    /// A key's memos and key bytes, read in from the image the first time.
    fn slot<'g>(&'g self, task: &'g Guard, key: &QueryKey) -> Option<&'g Slot> {
        if let Some(slot) = self.slots.get(task, key) {
            return Some(slot);
        }
        let image = self.image.as_ref()?;
        let found = image.find(key)?;
        let mut made = Vec::with_capacity(found.len());
        let slot = self.slots.get_or_insert_with(task, *key, || {
            for index in found.clone() {
                let memo = image.memo(index);
                let code = self.store.code(memo.schema, memo.producer);
                // SAFETY: the image's mapping is kept by the database as long as its store.
                made.push(unsafe {
                    self.store.make_mapped(memo.key, memo.fingerprint, code, memo.edges, memo.value)
                });
            }
            // SAFETY: as above.
            let key_bytes = image.key_bytes(found.start).map(|bytes| unsafe { Span::mapped(bytes) });
            self.store.slot(key_bytes, &made)
        });
        if slot.memos() != made.as_slice() {
            for memo in made {
                self.store.discard(memo);
            }
        }
        Some(slot)
    }

    /// Keeps `key`'s bytes beside its memos, unless they are kept already.
    fn learn_key<Q: Query>(&self, task: &Guard, query: QueryKey, key: &Q::Key) {
        if self.slot(task, &query).is_some_and(|slot| slot.key().is_some()) {
            return;
        }
        let bytes =
            cbor::to_vec(key).unwrap_or_else(|error| panic!("the key of query {} has no encoding: {error}", Q::NAME));
        self.slots.update(task, query, |slot| match slot {
            Some(slot) if slot.key().is_some() => None,
            Some(slot) => Some(self.store.slot(Some(self.store.key_bytes(&bytes)), slot.memos())),
            None => Some(self.store.slot(Some(self.store.key_bytes(&bytes)), &[])),
        });
    }

    /// Adds `made` to its key's memos and says which memo the key has for it: `made`, or an
    /// equal one it had already (then `made`, never published, is freed).
    fn add_memo(&self, task: &Guard, key: QueryKey, made: MemoId) -> MemoId {
        self.slot(task, &key);
        self.born.insert(task, key, self.generation.load(Ordering::SeqCst));
        let kept = Cell::new(made);
        self.slots.update(task, key, |slot| {
            let memos = slot.map_or(&[][..], Slot::memos);
            if let Some(&same) = memos.iter().find(|&&it| self.store.same(task, it, &self.store, made)) {
                kept.set(same);
                return None;
            }
            kept.set(made);
            let mut all = Vec::with_capacity(memos.len() + 1);
            all.extend_from_slice(memos);
            all.push(made);
            Some(self.store.slot(slot.and_then(Slot::share_key), &all))
        });
        let kept = kept.get();
        if kept != made {
            self.store.discard(made);
        }
        kept
    }

    /// An input's value in a view, read through the tree the first time the view asks.
    fn input<'g>(
        &'g self,
        task: &'g Guard,
        state: &'g ViewState,
        key: QueryKey,
        make: impl FnOnce() -> Input,
    ) -> &'g InputValue {
        if let Some(value) = state.inputs.get(task, &key) {
            return value;
        }
        let input = self.inputs.get_or_insert_with(task, key, make);
        state.inputs.get_or_insert_with(task, key, || self.read_input(state, input))
    }

    fn input_value<'g>(&'g self, task: &'g Guard, state: &'g ViewState, key: QueryKey) -> Option<&'g InputValue> {
        if let Some(value) = state.inputs.get(task, &key) {
            return Some(value);
        }
        let input = self.inputs.get(task, &key)?;
        Some(state.inputs.get_or_insert_with(task, key, || self.read_input(state, input)))
    }

    fn read_input(&self, state: &ViewState, input: &Input) -> InputValue {
        let absent = |what: &str| ContentId::of(ABSENT_DOMAIN, what.as_bytes()).fingerprint();
        match input {
            Input::File(path) => {
                let file = self.tree().file(state.view.root, path);
                let fingerprint = match &file {
                    Some((id, mode, _)) => Hasher::new(FILE_DOMAIN)
                        .id(id)
                        .update(&mode.git().to_le_bytes())
                        .finish()
                        .fingerprint(),
                    None => absent("file"),
                };
                InputValue { fingerprint, data: InputData::File(file) }
            }
            Input::Dir(path) => {
                let names = self.tree().list(state.view.root, path);
                let fingerprint = match &names {
                    Some(names) => {
                        let mut hasher = Hasher::new(NAMES_DOMAIN);
                        for (name, dir) in names {
                            hasher.part(name.as_bytes()).update(&[u8::from(*dir)]);
                        }
                        hasher.finish().fingerprint()
                    }
                    None => absent("directory"),
                };
                InputValue { fingerprint, data: InputData::Dir(names.map(Arc::from)) }
            }
            Input::Config => InputValue {
                fingerprint: ContentId::of(CONFIG_DOMAIN, &state.config).fingerprint(),
                data: InputData::Config,
            },
        }
    }

    /// A key's fingerprint in a view: an input's, or its memo's, verified or made.
    fn fingerprint_in(&self, task: &Guard, state: &ViewState, key: QueryKey, run: Run) -> Result<Option<Fingerprint>, Cancelled> {
        if is_input(key.kind) {
            return Ok(self.input_value(task, state, key).map(|value| value.fingerprint));
        }
        Ok(self.resolve(task, state, key, run, None)?.map(|memo| self.store.fingerprint(task, memo)))
    }

    /// The memo of `key` in a view: the one it resolved, a kept memo every edge of which still
    /// holds, or a new one made by running the query. `None` for a key the database can't run
    /// (a kind not registered, a key whose bytes it does not have).
    fn resolve(
        &self,
        task: &Guard,
        state: &ViewState,
        key: QueryKey,
        run: Run,
        typed: Option<&(dyn Any + Send + Sync)>,
    ) -> Result<Option<MemoId>, Cancelled> {
        let Some(info) = self.kinds.get(&key.kind) else { return Ok(None) };
        loop {
            if let Some(&memo) = state.results.get(task, &key) {
                return Ok(Some(memo));
            }
            let mut ran = None;
            self.single(key, run, || {
                ran = Some(self.resolve_held(task, state, info, key, run, typed)?);
                Ok(())
            })?;
            if let Some(found) = ran {
                return Ok(found);
            }
        }
    }

    /// Runs `work` as the one run of `key`, or waits (pumping) for the run under way and
    /// returns without running it, for the caller to look again. A wait that would close a
    /// cycle of tasks is [`Cancelled`].
    fn single(&self, key: QueryKey, run: Run, work: impl FnOnce() -> Result<(), Cancelled>) -> Result<(), Cancelled> {
        run.check(key);
        self.flights.get_or_run(key.task_key(), work)
    }

    /// [`Database::resolve`] while holding `key`.
    fn resolve_held(
        &self,
        task: &Guard,
        state: &ViewState,
        info: &KindInfo,
        key: QueryKey,
        run: Run,
        typed: Option<&(dyn Any + Send + Sync)>,
    ) -> Result<Option<MemoId>, Cancelled> {
        if let Some(&memo) = state.results.get(task, &key) {
            return Ok(Some(memo));
        }
        let _frame = Frame::push(key);
        let slot = self.slot(task, &key);
        let mut previous = None;
        for &memo in slot.map_or(&[][..], Slot::memos).iter().rev() {
            if self.store.hot(task, memo).code() != info.code {
                continue;
            }
            let change = match self.verify(task, state, memo, run)? {
                Verdict::Valid => return Ok(Some(*state.results.insert(task, key, memo))),
                Verdict::Changed(changed) => Some(changed),
                Verdict::Unknown => None,
            };
            previous.get_or_insert((self.store.fingerprint(task, memo), change));
        }
        let source = match (typed, slot.and_then(Slot::key)) {
            (Some(typed), _) => KeySource::Typed(typed),
            (None, Some(bytes)) => KeySource::Bytes(bytes),
            (None, None) => return Ok(None),
        };
        let Some((computed, recording)) = self.execute(task, state, info, run, source)? else {
            return Ok(None);
        };
        let made =
            recording.with(|edges| self.store.make(key, computed.fingerprint, info.code, edges, &computed.bytes));
        drop(recording);
        let memo = self.add_memo(task, key, made);
        state.reruns.insert(task, key, Rerun { previous });
        Ok(Some(*state.results.insert(task, key, memo)))
    }

    /// Whether every edge of `memo`, in order, still holds in the view: one sequential walk of
    /// its range of the edge segment, each edge's key and fingerprint read in place.
    fn verify(&self, task: &Guard, state: &ViewState, memo: MemoId, run: Run) -> Result<Verdict, Cancelled> {
        for edge in self.store.edges(task, memo) {
            let read = edge.read();
            match self.fingerprint_in(task, state, read, run)? {
                Some(now) if now == edge.saw() => {}
                Some(now) => return Ok(Verdict::Changed(Changed { key: read, was: edge.saw(), now })),
                None => return Ok(Verdict::Unknown),
            }
        }
        Ok(Verdict::Valid)
    }

    /// Runs the query, its edges recorded on this thread's edge stack while the returned
    /// [`Recording`] lives.
    fn execute(
        &self,
        task: &Guard,
        state: &ViewState,
        info: &KindInfo,
        run: Run,
        source: KeySource<'_>,
    ) -> Result<Option<(Computed, Recording)>, Cancelled> {
        let recording = Recording::start();
        let mut cx = Cx { db: self, task, state, run };
        let _inside = Inside::enter();
        let computed = (info.run)(&mut cx, source)?;
        Ok(computed.map(|computed| (computed, recording)))
    }

    /// The bytes of a memo's value, made again if it was evicted.
    fn value_of<'g>(
        &'g self,
        task: &'g Guard,
        state: &'g ViewState,
        key: QueryKey,
        memo: MemoId,
        run: Run,
        typed: &(dyn Any + Send + Sync),
    ) -> Result<&'g [u8], Cancelled> {
        loop {
            if let Some(bytes) = self.store.bytes(task, memo) {
                return Ok(bytes);
            }
            let info = &self.kinds[&key.kind];
            self.single(key, run, || {
                if self.store.bytes(task, memo).is_some() {
                    return Ok(());
                }
                let _frame = Frame::push(key);
                let (computed, _edges) = self
                    .execute(task, state, info, run, KeySource::Typed(typed))?
                    .expect("a query run with its typed key always answers");
                let fingerprint = self.store.fingerprint(task, memo);
                if computed.fingerprint != fingerprint {
                    panic!(
                        "query {} is not deterministic: made again after its value was evicted, \
                         {key:?} has fingerprint {:?}, not {fingerprint:?}",
                        info.name, computed.fingerprint
                    );
                }
                self.store.refill(task, memo, &computed.bytes);
                Ok(())
            })?;
        }
    }

    /// The fingerprint a view already has for `key`, computing nothing.
    fn known(&self, task: &Guard, state: &ViewState, key: QueryKey) -> Option<Fingerprint> {
        if is_input(key.kind) {
            state.inputs.get(task, &key).map(|value| value.fingerprint)
        } else {
            state.results.get(task, &key).map(|&memo| self.store.fingerprint(task, memo))
        }
    }

    /// What `changed.key` read that made it change from `changed.was`.
    fn why_changed(&self, task: &Guard, state: &ViewState, changed: Changed) -> Option<Changed> {
        if let Some(Rerun { previous: Some((was, Some(next))) }) = state.reruns.get(task, &changed.key)
            && *was == changed.was
        {
            return Some(*next);
        }
        let slot = self.slot(task, &changed.key)?;
        let old = slot.memos().iter().rev().copied().find(|&memo| self.store.fingerprint(task, memo) == changed.was)?;
        self.store.edges(task, old).iter().find_map(|edge| {
            let (read, saw) = (edge.read(), edge.saw());
            let now = self.known(task, state, read)?;
            (now != saw).then_some(Changed { key: read, was: saw, now })
        })
    }

    fn write_cold(&self, task: &Guard, memos: impl Iterator<Item = MemoId>, effects: &mut Vec<Effect>) {
        let dir = self.dir("move cold values");
        let mut bytes = Vec::new();
        let mut entries = Vec::new();
        for memo in memos {
            let Some((word, value)) = self.store.in_memory(task, memo) else { continue };
            bytes.resize(bytes.len().next_multiple_of(blaze_format::ALIGN), 0);
            let (at, len, id) = (bytes.len(), value.len(), value_id(value));
            bytes.extend_from_slice(value);
            entries.push(ColdEntry { memo, word, at, len, id });
        }
        if entries.is_empty() {
            return;
        }
        self.store.pin(entries.iter().map(|entry| entry.memo));
        let mut io = self.io.lock();
        let file = FileName::new(&format!("{dir}/cold/{}", io.cold_files))
            .expect("a name under the database's directory");
        io.cold_files += 1;
        let tag = io.tag(self.config.tags);
        effects.push(Effect::Write { file: file.clone(), bytes, tag });
        io.pending.insert(tag, Pending::ColdWrite { file, entries });
    }

    fn image_entries<'g>(&'g self, task: &'g Guard) -> Vec<image::Entry<'g>> {
        let mut keys: FxSet<QueryKey> = FxSet::default();
        self.slots.for_each(task, |key, _| {
            keys.insert(*key);
        });
        if let Some(image) = &self.image {
            keys.extend(image.keys());
        }
        let mut keys: Vec<QueryKey> = keys.into_iter().collect();
        keys.sort_unstable();
        keys.into_iter()
            .filter_map(|key| {
                let slot = self.slot(task, &key)?;
                if slot.memos().is_empty() {
                    return None;
                }
                let mut memos: Vec<(Fingerprint, ContentId, SchemaHash, &[EdgeRecord], MemoId)> = slot
                    .memos()
                    .iter()
                    .map(|&memo| {
                        let hot = self.store.hot(task, memo);
                        let (schema, producer) = self.store.parts(hot.code());
                        (hot.fingerprint(), producer, schema, self.store.edges(task, memo), memo)
                    })
                    .collect();
                memos.sort_by(|a, b| {
                    (a.0, a.1, a.2, a.3.len())
                        .cmp(&(b.0, b.1, b.2, b.3.len()))
                        .then_with(|| a.3.iter().map(EdgeRecord::order).cmp(b.3.iter().map(EdgeRecord::order)))
                });
                Some(image::Entry {
                    key,
                    memos: memos.into_iter().map(|(.., memo)| memo).collect(),
                    key_bytes: slot.key(),
                })
            })
            .collect()
    }

    fn image_inputs(&self, task: &Guard) -> Vec<(QueryKey, Input)> {
        let mut inputs = self.inputs.snapshot(task);
        inputs.sort_by_key(|(key, _)| *key);
        inputs
    }
}
