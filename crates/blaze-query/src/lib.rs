#![feature(allocator_api)]
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
mod subtree;
mod sync;
mod table;
pub mod tree;

#[cfg(test)]
mod tests;

use std::{
    any::{Any, TypeId},
    cell::{Cell, RefCell},
    collections::VecDeque,
    fmt,
    sync::{Arc, OnceLock},
};

use blaze_core::{Completion, Done, Effect, FileName, Mapping, Tag};
use blaze_format::{FormatError, Persist, SchemaHash, Writer};
use blaze_graph::pump::{self, Flights};
use blaze_ids::{
    ContentId, Fingerprint, Hasher, Idx,
    bits::{BitSet, Bits},
};
use blaze_image::{Builder, Corrupt, Image, Refused, SectionId};
use blaze_protocol::cbor;
use blaze_reclaim::{Grace, Guard};
use blaze_vfs::{View, ViewId};
use serde::{Deserialize, Serialize};

use crate::{
    image::ImageMap,
    memo::{MemoId, Slot, Store},
    segment::Span,
    sync::{
        atomic::{AtomicU64, Ordering},
        thread_local,
    },
    table::{Column, EdgeRecord, Edit, FxMap, FxSet, Map, Rows, ShortLock},
};
pub use crate::{
    subtree::TreeHash,
    tree::{MemTree, Tree},
};

/// A kind of query, by number. Numbers are given in `blaze-query`'s [`kinds`] for the
/// database's own, and by the crate that defines the rest (facts are `blaze-items`'s); one
/// number is one query for every blaze that reads a given schema.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[serde(transparent)]
pub struct QueryKind(pub u16);

/// The kind numbers of one crate's queries: `RANGE`, a constant for each kind, [`ALL`] with
/// each query's name, `of(name)`, and `name(kind)` from a table built when compiled, which
/// refuses a number outside `RANGE` or given twice.
#[macro_export]
macro_rules! query_kinds {
    (
        range: $range:expr, owner: $owner:literal;
        $($(#[$meta:meta])* $name:ident = $number:literal, $query:literal;)*
    ) => {
        pub use $crate::QueryKind;

        #[doc = ::core::concat!("The numbers ", $owner, " may give.")]
        pub const RANGE: ::core::ops::Range<u16> = $range;

        $($(#[$meta])* pub const $name: QueryKind = QueryKind($number);)*

        /// Every kind, with its query as its crate names it: a function's name, or
        /// `Type::method` for a method of a tracked `impl`.
        pub const ALL: &[(QueryKind, &str)] = &[$(($name, $query)),*];

        /// The kind of the query named `name`, as [`ALL`] names it.
        pub fn of(name: &str) -> Option<QueryKind> {
            match name {
                $($query => Some($name),)*
                _ => None,
            }
        }

        const NAMES: [Option<&str>; (RANGE.end - RANGE.start) as usize] = {
            let mut names = [None; (RANGE.end - RANGE.start) as usize];
            let mut at = 0;
            while at < ALL.len() {
                let (kind, name) = ALL[at];
                ::core::assert!(
                    RANGE.start <= kind.0 && kind.0 < RANGE.end,
                    ::core::concat!("a kind outside ", $owner, "'s range"),
                );
                let slot = (kind.0 - RANGE.start) as usize;
                ::core::assert!(names[slot].is_none(), "a kind given to two queries");
                names[slot] = Some(name);
                at += 1;
            }
            names
        };

        /// The query of `kind`, if it is this crate's: one bounds check and one load.
        pub fn name(kind: QueryKind) -> Option<&'static str> {
            NAMES.get(kind.0.wrapping_sub(RANGE.start) as usize).copied().flatten()
        }
    };
}

/// The database's own kinds, and the ranges given to others.
pub mod kinds {
    use super::QueryKind;

    /// A file of a view, by path: its content ID.
    pub const FILE: QueryKind = QueryKind(1);
    /// A directory of a view, by path: the content ID of its sorted names.
    pub const DIR: QueryKind = QueryKind(2);
    /// The view's configuration (target, profile, features asked for): its content ID.
    pub const CONFIG: QueryKind = QueryKind(3);
    /// The hash of everything below a directory of a view ([`crate::Cx::tree`]), one per
    /// directory: the database's own query, not an input.
    pub const TREE: QueryKind = QueryKind(4);
    /// A file of an immutable root other than the view's, by root and path: its content ID.
    pub const FILE_AT: QueryKind = QueryKind(5);
    /// A directory of an immutable root other than the view's, by root and path: the content
    /// ID of its sorted names.
    pub const DIR_AT: QueryKind = QueryKind(6);
    /// Facts (`blaze-items`): `0x1000..0x2000`.
    pub const FACTS: std::ops::Range<u16> = 0x1000..0x2000;
    /// The front end's queries (parse, expand, resolve, infer, MIR): `0x2000..0x4000`.
    pub const FRONT: std::ops::Range<u16> = 0x2000..0x4000;
    /// Codegen and link: `0x4000..0x5000`.
    pub const BACK: std::ops::Range<u16> = 0x4000..0x5000;
    /// Build scripts, proc macros and tests: `0x5000..0x6000`.
    pub const USER: std::ops::Range<u16> = 0x5000..0x6000;
    /// The workspace load and the unit graph (lane cargo): `0x6000..0x6800`.
    pub const CARGO: std::ops::Range<u16> = 0x6000..0x6800;
}

fn is_input(kind: QueryKind) -> bool {
    matches!(kind, kinds::FILE | kinds::DIR | kinds::CONFIG | kinds::FILE_AT | kinds::DIR_AT)
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
    pub saw: Fingerprint,
    pub read: QueryKey,
}

/// What the database keeps of one result.
#[derive(PartialEq, Eq, Debug)]
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
    /// A file under an immutable root of the tree other than the view's ([`Cx::file_at`]).
    FileAt { root: ContentId, path: String },
    /// A directory under an immutable root of the tree other than the view's ([`Cx::dir_at`]).
    DirAt { root: ContentId, path: String },
}

/// The domain of an input's path in its [`QueryKey`].
pub const PATH_DOMAIN: &str = "blaze v2 db input path 1";
/// The domain of an immutable root and a path under it in an input's [`QueryKey`].
pub const ROOT_PATH_DOMAIN: &str = "blaze v2 db input root path 1";
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
            Input::FileAt { root, path } => Input::file_at_key(*root, path),
            Input::DirAt { root, path } => Input::dir_at_key(*root, path),
        }
    }

    /// [`Input::FileAt`]'s key, made without the input.
    pub fn file_at_key(root: ContentId, path: &str) -> QueryKey {
        QueryKey { kind: kinds::FILE_AT, input: Hasher::new(ROOT_PATH_DOMAIN).id(&root).part(path.as_bytes()).finish() }
    }

    /// [`Input::DirAt`]'s key, made without the input.
    pub fn dir_at_key(root: ContentId, path: &str) -> QueryKey {
        QueryKey { kind: kinds::DIR_AT, input: Hasher::new(ROOT_PATH_DOMAIN).id(&root).part(path.as_bytes()).finish() }
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
    /// The subsystem its runs allocate for: each run is a `blaze_mem::frame::task` of it, so
    /// what the run's code takes through `blaze_mem::frame::current` counts there.
    const TAG: blaze_mem::Tag = blaze_mem::Tag::Query;
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
    // Kinds register at run time into a table of `Runner` fn pointers, so the caller's key is
    // erased and read back by one `TypeId` compare when the query runs.
    // dyn: only when a query runs, never on a lookup that finds its memo.
    Typed(&'a (dyn Any + Send + Sync)),
    Bytes(&'a [u8]),
}

/// A run's value, written once into a buffer of this thread's (see [`BUFFERS`]); copied once
/// into the thread's value segment by the store.
struct Computed {
    bytes: Vec<u8>,
    fingerprint: Fingerprint,
}

/// How many value buffers a thread keeps, and the largest it keeps: runs nest, each holding
/// one buffer until its value is copied, and one huge value should not stay pinned.
const BUFFERS_KEPT: usize = 8;
const BUFFER_KEPT_BYTES: usize = 1 << 20;

impl Drop for Computed {
    fn drop(&mut self) {
        let bytes = std::mem::take(&mut self.bytes);
        if (1..=BUFFER_KEPT_BYTES).contains(&bytes.capacity()) {
            // A thread being torn down has no buffers left to keep it in; the buffer is freed.
            let _ = BUFFERS.try_with(|buffers| {
                let mut buffers = buffers.borrow_mut();
                if buffers.len() < BUFFERS_KEPT {
                    buffers.push(bytes);
                }
            });
        }
    }
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
    let buffer = BUFFERS.with(|buffers| buffers.borrow_mut().pop()).unwrap_or_default();
    let mut out = Writer::with_buffer(<Q::Value as Persist>::SCHEMA, buffer);
    let root = value.write(&mut out);
    let bytes = out.finish(root);
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
    /// The tag of the frame task each run is ([`Query::TAG`]).
    tag: blaze_mem::Tag,
    /// The symbols a value of the kind names (`Persist::symbols`), marked live when an image
    /// is written.
    symbols: fn(&[u8], &mut Vec<u32>) -> Result<(), FormatError>,
}

/// The registered queries by kind number: a kind's row is at its number in one reserved
/// region, so finding it is a load of its live flag and of its row, independent of each
/// other, with no hash probed (every ask finds its kind). Written under `&mut`, read under
/// `&`, never removed.
struct Kinds {
    rows: Rows<KindInfo>,
    /// The kinds registered, for a settle to walk.
    numbers: Vec<QueryKind>,
}

impl Kinds {
    fn new() -> Kinds {
        // alloc: grows once per kind registered, at setup.
        Kinds { rows: Rows::sized(1 << 16), numbers: Vec::new() }
    }

    #[inline]
    fn get(&self, kind: QueryKind) -> Option<&KindInfo> {
        // SAFETY: a kind's row is written by `insert` under `&mut self`, which every `&self`
        // follows, and dropped only with the rows.
        unsafe { self.rows.written(u32::from(kind.0)) }
    }

    fn insert(&mut self, kind: QueryKind, info: KindInfo) {
        assert!(self.get(kind).is_none(), "kind {} registered twice", kind.0);
        // SAFETY: `&mut self` excludes every reader, and the row holds no value (just checked).
        unsafe { self.rows.put(u32::from(kind.0), info) };
        self.numbers.push(kind);
    }

    fn for_each_mut(&mut self, mut f: impl FnMut(&mut KindInfo)) {
        for &kind in &self.numbers {
            // SAFETY: registered, so written; `&mut self` excludes readers.
            f(unsafe { self.rows.get_mut(u32::from(kind.0)) });
        }
    }
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
    // alloc: empty until a run grows them, then reused by every run of the thread.
    static STACKS: RefCell<Stacks> = const { RefCell::new(Stacks { keys: Vec::new(), edges: Vec::new() }) };
    /// Value buffers a run writes into and gives back once the store copied its value, so a
    /// run allocates nothing for its value once they have grown.
    static BUFFERS: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) }; // alloc: as `STACKS`
    /// The file a run on this thread could not read, set as it stops the run with
    /// [`Cancelled`] and taken by the [`Pinned::get`] that run is under, which fails with it.
    static UNREADABLE: Cell<Option<Corrupt>> = const { Cell::new(None) };
}

/// What a [`Cancelled`] run's request fails with: the file it could not read, if that is why
/// it stopped.
fn stopped(_: Cancelled) -> DbError {
    match UNREADABLE.with(Cell::take) {
        Some(corrupt) => DbError::Corrupt(corrupt),
        None => DbError::Cancelled,
    }
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
    /// A run ahead of its asker ([`Cx::get_all`]): it never waits for a key another task runs
    /// but gives way at once, so the asker, which waits for it unseen by the cycle check, never
    /// waits on a chain that leads back to itself.
    ahead: bool,
}

impl Run {
    fn new() -> Run {
        Run { base: STACKS.with(|stacks| stacks.borrow().keys.len()), ahead: false }
    }

    fn ahead() -> Run {
        Run { ahead: true, ..Run::new() }
    }

    /// Panics, naming the chain, if `key` is running in this run already.
    fn check(self, key: QueryKey) {
        STACKS.with(|stacks| {
            let stacks = stacks.borrow();
            let running = &stacks.keys[self.base.min(stacks.keys.len())..];
            if running.contains(&key) {
                panic!(
                    "a query cycle: {} asks {key:?} while running it",
                    // alloc: the panic's message.
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
        self.found::<Q>(query, memo, key)
    }

    /// Records the edge on `query`, whose memo in the view is `memo`, and reads its value.
    fn found<Q: Query>(
        &mut self,
        query: QueryKey,
        memo: MemoId,
        key: &Q::Key,
    ) -> Result<<Q::Value as Persist>::Ref<'db>, Cancelled> {
        let (db, task, state) = (self.db, self.task, self.state);
        self.record(query, db.store.fingerprint(task, memo));
        let bytes = db.value_of(task, state, query, memo, self.run, key)?;
        Ok(read_value::<Q>(bytes))
    }

    /// Each key's value, as [`Cx::get`] of each in turn would give it, with the same edges in
    /// the same order (so verification and [`Database::why`] see what a loop of `get` makes).
    /// The keys the view has not resolved run at once: from the first, each other is a job of
    /// the plane's pool that resolves it ahead, while this task asks them in order and waits
    /// (pumping, a wait of the plane's) for one still running. A job ahead never waits: a key
    /// that needs one another task runs is left for this task to ask. The first key that is
    /// [`Cancelled`] cancels the call; what the jobs resolved stays as memos.
    ///
    /// When the view has resolved every key, it allocates only the list it returns.
    pub fn get_all<Q: Query>(
        &mut self,
        keys: &[Q::Key],
    ) -> Result<Vec<<Q::Value as Persist>::Ref<'db>>, Cancelled> {
        let (db, task, state) = (self.db, self.task, self.state);
        if db.kind_of::<Q>().is_none() {
            panic!("query {} (kind {}) is asked but was never registered", Q::NAME, Q::KIND.0);
        }
        // alloc: the values, one list a call.
        let mut values = Vec::with_capacity(keys.len());
        for (at, key) in keys.iter().enumerate() {
            let query = QueryKey { kind: Q::KIND, input: Q::input(key) };
            let Some(resolved) = state.results.get(task, &query) else {
                return self.get_rest::<Q>(&keys[at..], values);
            };
            let memo = resolved.memo;
            values.push(self.found::<Q>(query, memo, key)?);
        }
        Ok(values)
    }

    /// [`Cx::get_all`] from its first key the view has not resolved, `keys[0]`: the rest the
    /// view has not resolved either are resolved ahead by the pool while this task asks every
    /// key in order.
    fn get_rest<Q: Query>(
        &mut self,
        keys: &[Q::Key],
        mut values: Vec<<Q::Value as Persist>::Ref<'db>>,
    ) -> Result<Vec<<Q::Value as Persist>::Ref<'db>>, Cancelled> {
        let (db, task, state) = (self.db, self.task, self.state);
        let unresolved = |key: &Q::Key| state.results.get(task, &QueryKey { kind: Q::KIND, input: Q::input(key) }).is_none();
        if !keys[1..].iter().any(unresolved) {
            for key in keys {
                values.push(self.get::<Q>(key)?);
            }
            return Ok(values);
        }
        let near = blaze_graph::pool::group();
        blaze_graph::exec::shared().scope(|scope| {
            for key in keys[1..].iter().filter(|key| unresolved(*key)) {
                // A job that panics or gives way leaves its key unresolved: this task asks it
                // in its turn below, runs it itself and meets the same panic.
                scope.submit(near, move || db.ahead::<Q>(state, key), |_| {});
            }
            for key in keys {
                values.push(self.get::<Q>(key)?);
            }
            Ok(values)
        })
    }

    /// A file of the view: its content ID, mode and bytes; `None` for none. Records the edge.
    pub fn file(&mut self, path: &str) -> Result<Option<blaze_vfs::File<'db>>, Cancelled> {
        let key = Input::file_key(path);
        // alloc: the path, only when the database first sees the input.
        let value = self.db.input(self.task, self.state, key, || Input::File(path.to_owned()))?;
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
        // alloc: the path, only when the database first sees the input.
        let value = self.db.input(self.task, self.state, key, || Input::Dir(path.to_owned()))?;
        self.record(key, value.fingerprint);
        Ok(match &value.data {
            InputData::Dir(Some(names)) => Some(&**names),
            _ => None,
        })
    }

    /// The file at `path` under `root`, a root of the database's tree other than the view's
    /// (a registry package's source, the standard library's `library/`): its content ID, mode
    /// and bytes; `None` for none. A root never changes, so this is a pure function of
    /// `(root, path)`. Records the edge, keyed by both, whose fingerprint is the file's.
    pub fn file_at(&mut self, root: ContentId, path: &str) -> Result<Option<blaze_vfs::File<'db>>, Cancelled> {
        let key = Input::file_at_key(root, path);
        // alloc: the path, only when the database first sees the input.
        let value = self.db.input(self.task, self.state, key, || Input::FileAt { root, path: path.to_owned() })?;
        self.record(key, value.fingerprint);
        Ok(match &value.data {
            InputData::File(Some((id, mode, bytes))) => {
                Some(blaze_vfs::File { id: *id, mode: *mode, bytes: &**bytes })
            }
            _ => None,
        })
    }

    /// The names in a directory under `root`, as [`Cx::dir`] and [`Cx::file_at`]. Records
    /// the edge.
    pub fn dir_at(&mut self, root: ContentId, path: &str) -> Result<Option<&'db [(String, bool)]>, Cancelled> {
        let key = Input::dir_at_key(root, path);
        // alloc: the path, only when the database first sees the input.
        let value = self.db.input(self.task, self.state, key, || Input::DirAt { root, path: path.to_owned() })?;
        self.record(key, value.fingerprint);
        Ok(match &value.data {
            InputData::Dir(Some(names)) => Some(&**names),
            _ => None,
        })
    }

    /// The hash of everything below `dir` in the view, `None` for no directory there; with
    /// `package`, a package's tree as cargo watches it (no name starting with `.`, no top
    /// `target`). One query per directory, so an edit reruns only the directories above it.
    /// Records one edge, on `dir`'s query.
    pub fn tree(&mut self, dir: &str, package: bool) -> Result<Option<[u8; 32]>, Cancelled> {
        // alloc: the key's path, once per call; its runs make their children's the same way.
        let key = subtree::SubtreeKey { dir: dir.trim_end_matches('/').to_owned(), package, top: true };
        self.get::<subtree::Subtree>(&key)
    }

    /// The view's configuration, as it was opened with ([`Database::open_with`]). Records the
    /// edge.
    pub fn config(&mut self) -> Result<&'db [u8], Cancelled> {
        let state = self.state;
        let key = Input::config_key();
        let value = self.db.input(self.task, state, key, || Input::Config)?;
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
    /// The first is what `query` read that changed, the last an input.
    pub chain: Vec<Changed>,
    /// The input at the end, as the view names it.
    pub input: Option<Input>,
    pub query: QueryKey,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Changed {
    pub key: QueryKey,
    pub now: Fingerprint,
    pub was: Fingerprint,
}

/// Why the database could not answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DbError {
    Cancelled,
    /// The view is not open.
    NoView(ViewId),
    /// A query kind asked that no one registered.
    Unknown(QueryKind),
    /// The image has no database section of this layout.
    Refused(Refused),
    /// A group of the image failed its check: one the database reads whole at start (its
    /// inputs), or the one holding a file a run read. Never kept: the next request reads it
    /// again.
    Corrupt(Corrupt),
    /// A database section of a shape [`Database::write_image`] never writes.
    Invalid { section: SectionId, what: &'static str },
    /// The symbol table was not restored from this image (`intern::restore_symbols`), so a
    /// value naming a symbol by ID can't be read.
    Symbols,
}

impl DbError {
    /// Whether the database could not use an image, and starts empty instead (rebuilt from
    /// the store's results).
    pub fn is_image(&self) -> bool {
        matches!(
            self,
            DbError::Refused(_) | DbError::Corrupt(_) | DbError::Invalid { .. } | DbError::Symbols
        )
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Cancelled => write!(f, "cancelled"),
            DbError::NoView(view) => write!(f, "view {view:?} is not open"),
            DbError::Unknown(kind) => write!(f, "no query of kind {} is registered", kind.0),
            DbError::Refused(refused) => write!(f, "the daemon image: {refused}"),
            DbError::Corrupt(corrupt) => write!(f, "the daemon image: {corrupt}"),
            DbError::Invalid { section, what } => write!(f, "the daemon image's {section}: {what}"),
            DbError::Symbols => write!(f, "the symbol table was not restored from the daemon image"),
        }
    }
}

/// How a database reads and where it keeps what it writes.
#[derive(Clone)]
pub struct Config {
    /// The files of every view; `None` for a database no query of which reads a file.
    // The file source, chosen once when the database is made: the VFS's reader, or a `MemTree`.
    // dyn: called once per input per view, beside hashing what it returns.
    pub tree: Option<Arc<dyn Tree>>,
    /// Its directory under the driver's root, for its cold packs (`<dir>/cold/<n>`). `None`
    /// keeps every value in memory. The image is the daemon's to write ([`Database::write_image`]).
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
/// A result is one 64-byte line of row (the key, the memo's ID and its fingerprint) and at
/// most 16 bytes of places; finding one is a shard's table load, about 1.5 place loads and one
/// row read, with no hash computed. Verifying an edge to a key the view resolved reads that
/// line alone, never the memo's.
struct ViewState {
    id: ViewId,
    view: View,
    config: Arc<[u8]>,
    results: Map<Resolved>,
    inputs: Map<InputValue>,
    reruns: Map<Rerun>,
}

/// How many edges ahead [`Database::verify`] starts a lookup: about the misses a core keeps in
/// flight (20) over the two each lookup takes.
const AHEAD: usize = 8;

/// From how many memos [`Database::verify`] prefetches: 2^16 memos are about 8 MB of memo and
/// result lines, past a core's L2, so lookups start to miss.
const PREFETCH_FROM: u32 = 1 << 16;

/// A key's memo in a view, with the memo's fingerprint beside it.
#[derive(Clone, Copy)]
#[repr(C)]
struct Resolved {
    memo: MemoId,
    /// [`Fingerprint`], as two words so the entry is 8-byte aligned.
    fingerprint: [u64; 2],
}

impl Resolved {
    fn new(store: &Store, task: &Guard, memo: MemoId) -> Resolved {
        let Fingerprint(fingerprint) = store.fingerprint(task, memo);
        Resolved { memo, fingerprint: [fingerprint as u64, (fingerprint >> 64) as u64] }
    }

    fn fingerprint(&self) -> Fingerprint {
        Fingerprint(u128::from(self.fingerprint[0]) | u128::from(self.fingerprint[1]) << 64)
    }
}

const _: () = assert!(size_of::<table::Entry<Resolved>>() == 64);
const _: () = assert!(size_of::<table::Entry<u64>>() == 48);

impl ViewState {
    fn new(id: ViewId, view: View, config: Arc<[u8]>) -> ViewState {
        ViewState { id, view, config, results: Map::hot(), inputs: Map::new(), reruns: Map::new() }
    }

    fn sweep(&self) {
        self.results.sweep();
        self.inputs.sweep();
        self.reruns.sweep();
    }

    fn reclaim(&mut self) {
        self.results.reclaim();
        self.inputs.reclaim();
        self.reruns.reclaim();
    }

    /// Empties it for another view, its maps' tables kept: no reader can be in it.
    fn reset(&mut self) {
        self.results.reset();
        self.inputs.reset();
        self.reruns.reset();
    }

    /// Makes it the state of `view`, as `ViewState::new` would.
    fn reuse(&mut self, id: ViewId, view: View, config: &[u8]) {
        self.id = id;
        self.view = view;
        if *self.config != *config {
            // alloc: a configuration other than the one of the last view this state served.
            self.config = Arc::from(config);
        }
    }
}

/// Closed views' states, kept to be the next views opened: each held until no task can still
/// read it, then emptied, its maps' tables kept, so opening a view and resolving in it
/// allocates nothing once the database has had as many views open at once.
#[derive(Default)]
struct SpareViews {
    /// Oldest first.
    held: VecDeque<(Grace, Box<ViewState>)>,
    ready: Vec<Box<ViewState>>,
}

/// Emptied states kept; one more closed is freed.
const SPARE_VIEWS: usize = 8;

impl SpareViews {
    /// Empties those whose grace has passed, keeping up to [`SPARE_VIEWS`] and freeing the rest.
    fn sweep(&mut self) {
        while self.held.front().is_some_and(|(grace, _)| grace.passed()) {
            let Some((_, mut state)) = self.held.pop_front() else { break };
            if self.ready.len() < SPARE_VIEWS {
                state.reset();
                self.ready.push(state);
            }
        }
    }

    /// An emptied state, emptying first those whose grace has passed.
    fn take(&mut self) -> Option<Box<ViewState>> {
        self.sweep();
        self.ready.pop()
    }

    /// [`SpareViews::sweep`] of every state held, past its grace or not: for a database
    /// borrowed `&mut`, which no task reads.
    fn reclaim(&mut self) {
        while let Some((_, mut state)) = self.held.pop_front() {
            if self.ready.len() < SPARE_VIEWS {
                state.reset();
                self.ready.push(state);
            }
        }
    }
}

/// Why a key ran in a view: the newest memo it had then (its fingerprint) and that memo's
/// first edge that differed, `None` where the edge's query could not be run by key; no memo
/// at all for a first run.
#[derive(Clone, Copy)]
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
    File(Option<(ContentId, blaze_vfs::Mode, blaze_vfs::Bytes)>),
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
    Remove { file: FileName },
}

impl Pending {
    /// What the database asked, and of which file: for the message of a failure.
    fn what(&self) -> (&'static str, &FileName) {
        match self {
            Pending::ColdWrite { file, .. } => ("write its cold pack", file),
            Pending::ColdMap { file, .. } => ("map its cold pack", file),
            Pending::Remove { file } => ("remove", file),
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
    kinds: Kinds,
    /// Every key's memos made or touched since start; the rest are in the image.
    slots: Map<Slot>,
    /// What each input key a memo read names, to read it again in another view.
    inputs: Map<Input>,
    /// By [`ViewId`]; a closed one is taken out.
    views: Column<ViewState>,
    /// One run of a key at a time, whatever view asked, on the plane's wait graph (#1301).
    flights: Flights<()>,
    /// Closed views, until every task that could hold them has ended, then kept for the next
    /// views opened.
    spare_views: ShortLock<SpareViews>,
    /// Bumped by each collection; a key given a memo since one started is kept by it whole.
    generation: AtomicU64,
    /// The generation each key was last given a memo in: 48 bytes of row a key.
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
        UNREADABLE.with(|it| it.set(None));
        let memo = db.find::<Q>(task, state, query, run, key).map_err(stopped)?;
        let bytes = db.value_of(task, state, query, memo, run, key).map_err(stopped)?;
        Ok(read_value::<Q>(bytes))
    }
}

impl Database {
    fn blank(config: Config) -> Database {
        let mut db = Database {
            config,
            kinds: Kinds::new(),
            slots: Map::hot(),
            inputs: Map::new(),
            views: Column::new(),
            flights: Flights::new(),
            spare_views: ShortLock::default(),
            generation: AtomicU64::new(0),
            born: Map::new(),
            store: Store::new(),
            image: None,
            io: ShortLock::default(),
        };
        db.register::<subtree::Subtree>();
        db
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
    pub fn from_image(image: Arc<Image<Mapping>>) -> Result<Database, DbError> {
        Database::from_image_with(Config::memory(), image)
    }

    /// The database in a mapped image, used in place, reading and writing as `config` says.
    /// The daemon restores the symbol table from the same image first
    /// (`intern::restore_symbols`), since values name symbols by their IDs in it; on any
    /// error the database starts empty.
    pub fn from_image_with(config: Config, image: Arc<Image<Mapping>>) -> Result<Database, DbError> {
        if intern::restored_from() != Some(image.hash()) {
            return Err(DbError::Symbols);
        }
        Database::open_image(config, image)
    }

    /// [`Database::from_image_with`] with this process's symbol table as it is: for an image
    /// this process wrote (a settle), whose symbols are the ones it holds.
    fn open_image(config: Config, image: Arc<Image<Mapping>>) -> Result<Database, DbError> {
        let image = ImageMap::open(image)?;
        let inputs = image.inputs().map_err(DbError::Corrupt)?;
        let db = Database::blank(config);
        {
            let task = blaze_reclaim::enter();
            for (key, input) in inputs {
                db.inputs.insert(&task, key, input);
            }
        }
        Ok(Database { image: Some(image), ..db })
    }

    /// The groups of the image found corrupt since the last call, each once: their keys were
    /// computed again. For the daemon to report by section and group.
    pub fn take_corrupt(&self) -> Vec<Corrupt> {
        self.image.as_ref().map(ImageMap::take_corrupt).unwrap_or_default()
    }

    pub fn register<Q: Query>(&mut self) {
        assert!(!is_input(Q::KIND), "query {} takes kind {}, an input's", Q::NAME, Q::KIND.0);
        if let Some(info) = self.kinds.get(Q::KIND) {
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
                tag: Q::TAG,
                symbols: <Q::Value as Persist>::symbols,
            },
        );
    }

    pub fn open(&self, view: View) -> ViewId {
        self.open_with(view, &[])
    }

    /// Opens `view` with its configuration (target, profile, features asked for), what
    /// [`Cx::config`] reads.
    pub fn open_with(&self, view: View, config: &[u8]) -> ViewId {
        let spare = self.spare_views.lock().take();
        Idx::from_raw(self.views.push_boxed(|index| match spare {
            Some(mut state) => {
                state.reuse(Idx::from_raw(index), view, config);
                state
            }
            // alloc: a view's state, until as many views were closed as are open at once.
            None => Box::new(ViewState::new(Idx::from_raw(index), view, Arc::from(config))),
        }))
    }

    pub fn close(&self, view: ViewId) {
        if let Some(state) = self.views.take(view.raw()) {
            // The take unlinked it: a grace begun now covers every task that could read it.
            self.spare_views.lock().held.push_back((Grace::start(), state));
        }
    }

    /// Emptied view states ready for the next views opened.
    #[cfg(test)]
    fn ready_views(&self) -> usize {
        let mut spare = self.spare_views.lock();
        spare.sweep();
        spare.ready.len()
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
            // alloc: an inspection's answer, owned by its caller.
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
        // alloc: an inspection's answer, owned by its caller.
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
        blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
            // Before the views are walked: a memo unlinked earlier is checked against what
            // they hold now (see `memo`'s comment).
            let mut dying: Vec<MemoId, _> = Vec::new_in(frame);
            self.store.to_check(&mut dying);
            // A bit a memo each.
            let marks = || {
                let mut marks = Bits::<_, MemoId>::from_words(Vec::<u64, _>::new_in(frame));
                marks.grow(self.store.end() as usize);
                marks
            };
            let (mut reached, mut hot) = (marks(), marks());
            let mut inputs_kept = hashbrown::HashSet::with_hasher_in(rustc_hash::FxBuildHasher, frame);
            let mut work: Vec<MemoId, _> = Vec::new_in(frame);
            self.views.for_each(&task, |state| {
                state.results.for_each(&task, |_, resolved| {
                    let memo = resolved.memo;
                    hot.insert_growing(memo);
                    work.push(memo);
                });
                state.inputs.for_each(&task, |key, _| {
                    inputs_kept.insert(*key);
                });
            });
            self.store.checked(&dying, |memo| hot.contains(memo));
            for key in kept {
                if is_input(key.kind) {
                    inputs_kept.insert(*key);
                } else if let Some(slot) = self.slot(&task, key) {
                    work.extend_from_slice(slot.memos());
                }
            }
            while let Some(memo) = work.pop() {
                if !reached.insert_growing(memo) {
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
                        work.extend(
                            slot.memos().iter().copied().filter(|&it| self.store.fingerprint(&task, it) == saw),
                        );
                    }
                }
            }

            let young = |key: &QueryKey| self.born.get(&task, key).is_some_and(|born| *born > started);
            let mut unlinked: Vec<MemoId, _> = Vec::new_in(frame);
            // Reused for every slot.
            let mut live: Vec<MemoId, _> = Vec::new_in(frame);
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
                    live.clear();
                    live.extend(memos.iter().copied().filter(|&memo| reached.contains(memo)));
                    if live.len() == memos.len() {
                        Edit::Keep
                    } else if !live.is_empty() {
                        Edit::Replace(self.store.slot(slot.share_key(), &live))
                    } else if self.image.as_ref().is_some_and(|image| !matches!(image.find(key), Ok(None))) {
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
            self.store.unlinked(&unlinked);
            if let Some(image) = &self.image {
                image.for_each_key(|key| {
                    self.slots.get_or_insert_with(&task, key, Slot::empty);
                });
            }
            self.born.retain(&task, |_, born| *born > started);
            self.inputs.retain(&task, |key, _| inputs_kept.contains(key));

            if self.config.dir.is_some() {
                let mut cold: Vec<(QueryKey, Fingerprint, MemoId), _> = Vec::new_in(frame);
                cold.extend(
                    reached
                        .iter()
                        .filter(|&memo| !hot.contains(memo) && self.store.stored(&task, memo) == Stored::Memory)
                        .map(|memo| (self.store.key(&task, memo), self.store.fingerprint(&task, memo), memo)),
                );
                cold.sort_unstable();
                self.write_cold(&task, cold.into_iter().map(|(_, _, memo)| memo), effects);
            }
            self.collect_retired(&task);
        });
    }

    /// How many closed views and values let go of are still held, after freeing those past
    /// their grace.
    #[cfg(test)]
    fn held(&self) -> usize {
        let mut spare = self.spare_views.lock();
        spare.sweep();
        spare.held.len() + self.store.held()
    }

    /// Frees what the database's tables and store let go of whose grace has passed: the
    /// tables' rows first, as a slot's row holds spans of the store's segments.
    fn collect_retired(&self, task: &Guard) {
        self.spare_views.lock().sweep();
        self.slots.sweep();
        self.inputs.sweep();
        self.born.sweep();
        self.views.for_each(task, ViewState::sweep);
        self.store.sweep();
    }

    /// Writes the whole database into the daemon's image: its five sections into `builder`,
    /// and each symbol a value names into `live`, so the symbol table the daemon writes last
    /// (`intern::write_symbols`) keeps it. The daemon writes the image as one `Effect::Write`,
    /// maps it and hands it to [`Database::settle`]. Every kind with a memo must be registered,
    /// as its values' symbols are found through it: [`DbError::Unknown`] names one that is
    /// not, before anything is written.
    pub fn write_image(&self, builder: &mut Builder, live: &mut intern::Live) -> Result<(), DbError> {
        let task = blaze_reclaim::enter();
        let entries = self.image_entries(&task);
        let unknown = entries.keys.iter().find(|entry| {
            !entry.memos.is_empty() && !is_input(entry.key.kind) && self.kinds.get(entry.key.kind).is_none()
        });
        if let Some(entry) = unknown {
            return Err(DbError::Unknown(entry.key.kind));
        }
        // alloc: one list of a value's symbols, reused for every value of the image.
        let mut ids = Vec::new();
        image::write(&task, &self.store, &entries, &self.image_inputs(&task), builder, |key, bytes| {
            let Some(info) = self.kinds.get(key.kind) else { return };
            ids.clear();
            if let Err(error) = (info.symbols)(bytes, &mut ids) {
                panic!("a value of query {} does not read back as its schema: {error}", info.name)
            }
            for &id in &ids {
                live.mark_id(id);
            }
        });
        Ok(())
    }

    pub fn complete(&self, completion: Completion, effects: &mut Vec<Effect>) {
        let mut io = self.io.lock();
        let Some(pending) = io.pending.remove(&completion.tag) else {
            panic!("the database did not ask for {:?}, which completed as {:?}", completion.tag, completion.done)
        };
        match (pending, completion.done) {
            (Pending::ColdWrite { file, entries }, Done::Written) => {
                let tag = io.tag(self.config.tags);
                effects.push(Effect::Map { file: file.clone(), tag }); // clone: a name, once an I/O
                io.pending.insert(tag, Pending::ColdMap { file, entries });
            }
            (Pending::ColdMap { file, entries }, Done::Mapped(mapping)) => {
                drop(io);
                let task = blaze_reclaim::enter();
                for entry in entries {
                    let bytes = mapping.bytes().get(entry.at..entry.at + entry.len);
                    // The value still in memory is compared byte for byte (a `memcmp`, not a
                    // second hash); one swapped out since is checked by its content ID.
                    let wrote = self.store.in_memory(&task, entry.memo).filter(|(word, _)| *word == entry.word);
                    let holds = |bytes: &&[u8]| match wrote {
                        Some((_, wrote)) => *bytes == wrote,
                        None => value_id(bytes) == entry.id,
                    };
                    let Some(bytes) = bytes.filter(holds) else {
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
            (Pending::Remove { .. }, Done::Removed) => {}
            (pending, Done::Failed(error)) => {
                let (doing, file) = pending.what();
                panic!("the database could not {doing} {file}: {error}")
            }
            (pending, done) => {
                let (doing, file) = pending.what();
                panic!("the database asked to {doing} {file} and was told {done:?}")
            }
        }
    }
}

impl Database {
    /// Whether a completion is of an effect this asked for.
    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.config.tags) < 1 << 32
    }

    /// The settle swap (#1299): once the image [`Database::write_image`] wrote is on disk and
    /// mapped, the database becomes the image's, used in place, and every arena of the cold
    /// build is dropped. Open views stay open with what they resolved; queries stay
    /// registered. On an error the database is left as it was.
    pub fn settle(&mut self, image: Arc<Image<Mapping>>, effects: &mut Vec<Effect>) -> Result<(), DbError> {
        // Settle runs once per cold build, and copies each view's inputs (`Arc`s) and reruns
        // into the new database.
        // clone: once a settle, as is every clone and box below.
        let mut new = Database::open_image(self.config.clone(), image)?;
        std::mem::swap(&mut new.kinds, &mut self.kinds);
        let store = &new.store;
        new.kinds.for_each_mut(|info| info.code = store.code(info.schema, info.producer));
        let (next, views) = self.views.take_all();
        let task = blaze_reclaim::enter();
        let views = views
            .into_iter()
            .map(|old| {
                let fresh = ViewState::new(old.id, old.view, old.config.clone()); // clone: see above
                old.inputs.for_each(&task, |key, value| {
                    fresh.inputs.insert(&task, *key, value.clone()); // clone: see above
                });
                old.reruns.for_each(&task, |key, rerun| {
                    fresh.reruns.insert(&task, *key, *rerun);
                });
                old.results.for_each(&task, |key, resolved| {
                    let same = new.slot(&task, key).and_then(|slot| {
                        slot.memos().iter().copied().find(|&it| new.store.same(&task, it, &self.store, resolved.memo))
                    });
                    if let Some(same) = same {
                        fresh.results.insert(&task, *key, Resolved { memo: same, ..*resolved });
                    }
                });
                Box::new(fresh) // alloc: see above
            })
            .collect(); // alloc: see above
        drop(task);
        new.views.restore(next, views, |state| state.id.raw());
        let old_io = std::mem::take(self.io.get_mut());
        let new_io = new.io.get_mut();
        new_io.next = old_io.next;
        new_io.cold_files = old_io.cold_files;
        new_io.pending = old_io.pending;
        for (file, _) in &old_io.packs {
            let tag = new_io.tag(new.config.tags);
            effects.push(Effect::Remove { file: file.clone(), tag }); // clone: see above
            new_io.pending.insert(tag, Pending::Remove { file: file.clone() }); // clone: see above
        }
        *self = new;
        // The old store, whose rows named the packs' bytes, is gone with the old database.
        drop(old_io.packs);
        Ok(())
    }

    /// Frees everything let go of, past its grace or not: with `&mut`, nothing reads it.
    pub fn reclaim(&mut self) {
        self.spare_views.get_mut().reclaim();
        self.slots.reclaim();
        self.inputs.reclaim();
        self.born.reclaim();
        // alloc: one bitset a reclaim, 1 bit a memo.
        let mut held = BitSet::<MemoId>::new(self.store.end() as usize);
        {
            let task = blaze_reclaim::enter();
            self.views.for_each(&task, |state| {
                state.results.for_each(&task, |_, resolved| {
                    held.insert_growing(resolved.memo);
                });
            });
        }
        self.views.values_mut().for_each(ViewState::reclaim);
        self.store.reclaim(|memo| held.contains(memo));
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

    // dyn: see `Config::tree`.
    fn tree(&self) -> &dyn Tree {
        self.config.tree.as_deref().unwrap_or_else(|| {
            panic!("the database has no file tree to read a view's files from: make it with Database::new")
        })
    }

    fn view_state<'g>(&'g self, task: &'g Guard, view: ViewId) -> Option<&'g ViewState> {
        self.views.get(task, view.raw())
    }

    /// `Q`'s kind, if registered; a panic if another query took its kind.
    fn kind_of<Q: Query>(&self) -> Option<&KindInfo> {
        let info = self.kinds.get(Q::KIND)?;
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
        if let Some(resolved) = state.results.get(task, &query) {
            return Ok(resolved.memo);
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
        // A key whose memos are in a corrupt group has none from the image: it is computed
        // again, and the empty slot keeps the group from being read again.
        let read = || -> Result<Option<_>, Corrupt> {
            let Some(found) = image.find(key)? else { return Ok(None) };
            // alloc: once a key read in from the image, as is the slot's list of its memos.
            let mut memos = Vec::with_capacity(found.len());
            for index in found.start..found.end {
                memos.push(image.memo(index)?);
            }
            Ok(Some((memos, image.key_bytes(found.start)?)))
        };
        let (memos, key_bytes) = match read() {
            Ok(Some(read)) => read,
            Ok(None) => return None,
            Err(corrupt) => {
                image.note(corrupt);
                return Some(self.slots.get_or_insert_with(task, *key, Slot::empty));
            }
        };
        let mut made = Vec::with_capacity(memos.len());
        let slot = self.slots.get_or_insert_with(task, *key, || {
            for memo in &memos {
                let code = self.store.code(memo.schema, memo.producer);
                // SAFETY: the image's mapping is kept by the database as long as its store.
                made.push(unsafe {
                    self.store.make_mapped(memo.key, memo.fingerprint, code, memo.edges, memo.value)
                });
            }
            // SAFETY: as above.
            let key_bytes = key_bytes.map(|bytes| unsafe { Span::mapped(bytes) });
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
        let mut bytes = blaze_mem::scratch::lend_bytes();
        cbor::append(&mut bytes, key)
            .unwrap_or_else(|error| panic!("the key of query {} has no encoding: {error}", Q::NAME));
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
        // Only ever raised: a giver that loaded the generation before `collect` bumped it must
        // not lower the stamp of one that loaded it after, or `collect` drops a young memo.
        let now = self.generation.load(Ordering::SeqCst);
        self.born.update(task, key, |born| born.is_none_or(|born| *born < now).then_some(now));
        let kept = Cell::new(made);
        self.slots.update(task, key, |slot| {
            let memos = slot.map_or(&[][..], Slot::memos);
            if let Some(&same) = memos.iter().find(|&&it| self.store.same(task, it, &self.store, made)) {
                kept.set(same);
                return None;
            }
            kept.set(made);
            Some(blaze_mem::scratch::with_u32s(|all| {
                all.extend_from_slice(memos);
                all.push(made);
                self.store.slot(slot.and_then(Slot::share_key), all)
            }))
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
    ) -> Result<&'g InputValue, Cancelled> {
        if let Some(value) = state.inputs.get(task, &key) {
            return Ok(value);
        }
        let input = self.inputs.get_or_insert_with(task, key, make);
        self.keep_input(task, state, key, input)
    }

    fn input_value<'g>(
        &'g self,
        task: &'g Guard,
        state: &'g ViewState,
        key: QueryKey,
    ) -> Result<Option<&'g InputValue>, Cancelled> {
        if let Some(value) = state.inputs.get(task, &key) {
            return Ok(Some(value));
        }
        let Some(input) = self.inputs.get(task, &key) else { return Ok(None) };
        self.keep_input(task, state, key, input).map(Some)
    }

    /// Reads `input` in the view and keeps it there (a racing read's, if it kept one first). A
    /// file the tree can't read stops the run as a cancellation does, keeping nothing: its
    /// request fails naming the image's section and group ([`UNREADABLE`]), and the next one
    /// reads the file again.
    fn keep_input<'g>(
        &'g self,
        task: &'g Guard,
        state: &'g ViewState,
        key: QueryKey,
        input: &Input,
    ) -> Result<&'g InputValue, Cancelled> {
        match self.read_input(state, input) {
            Ok(value) => Ok(state.inputs.get_or_insert_with(task, key, || value)),
            Err(corrupt) => {
                UNREADABLE.with(|it| it.set(Some(corrupt)));
                Err(Cancelled)
            }
        }
    }

    fn read_input(&self, state: &ViewState, input: &Input) -> Result<InputValue, Corrupt> {
        let absent = |what: &str| ContentId::of(ABSENT_DOMAIN, what.as_bytes()).fingerprint();
        let root = match input {
            Input::FileAt { root, .. } | Input::DirAt { root, .. } => *root,
            _ => state.view.root,
        };
        match input {
            Input::File(path) | Input::FileAt { path, .. } => {
                let file = self.tree().file(root, path)?;
                let fingerprint = match &file {
                    Some((id, mode, _)) => Hasher::new(FILE_DOMAIN)
                        .id(id)
                        .update(&mode.git().to_le_bytes())
                        .finish()
                        .fingerprint(),
                    None => absent("file"),
                };
                Ok(InputValue { fingerprint, data: InputData::File(file) })
            }
            Input::Dir(path) | Input::DirAt { path, .. } => {
                let mut names = self.tree().list(root, path);
                // Once per directory per view, so neither `Cx::dir`'s callers nor the
                // fingerprint below depend on the tree's listing order.
                if let Some(names) = &mut names
                    && !names.is_sorted_by(|a, b| a.0 <= b.0)
                {
                    names.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                }
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
                Ok(InputValue { fingerprint, data: InputData::Dir(names.map(Arc::from)) })
            }
            Input::Config => Ok(InputValue {
                fingerprint: ContentId::of(CONFIG_DOMAIN, &state.config).fingerprint(),
                data: InputData::Config,
            }),
        }
    }

    /// A key's fingerprint in a view: an input's, or its memo's, verified or made.
    fn fingerprint_in(&self, task: &Guard, state: &ViewState, key: QueryKey, run: Run) -> Result<Option<Fingerprint>, Cancelled> {
        if is_input(key.kind) {
            return Ok(self.input_value(task, state, key)?.map(|value| value.fingerprint));
        }
        if let Some(resolved) = state.results.get(task, &key) {
            return Ok(Some(resolved.fingerprint()));
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
        typed: Option<&(dyn Any + Send + Sync)>, // dyn: see `KeySource::Typed`
    ) -> Result<Option<MemoId>, Cancelled> {
        if let Some(resolved) = state.results.get(task, &key) {
            return Ok(Some(resolved.memo));
        }
        let Some(info) = self.kinds.get(key.kind) else { return Ok(None) };
        loop {
            let mut ran = None;
            self.single(key, run, || {
                ran = Some(self.resolve_held(task, state, info, key, run, typed)?);
                Ok(())
            })?;
            if let Some(found) = ran {
                return Ok(found);
            }
            if let Some(resolved) = state.results.get(task, &key) {
                return Ok(Some(resolved.memo));
            }
        }
    }

    /// Runs `work` as the one run of `key`, or waits (pumping) for the run under way and
    /// returns without running it, for the caller to look again. A wait that would close a
    /// cycle of tasks is [`Cancelled`]; a run ahead never waits, and a key another task runs
    /// is [`Cancelled`] to it at once.
    fn single(&self, key: QueryKey, run: Run, work: impl FnOnce() -> Result<(), Cancelled>) -> Result<(), Cancelled> {
        run.check(key);
        if run.ahead {
            return self.flights.run_if_idle(key.task_key(), work).unwrap_or(Err(Cancelled));
        }
        self.flights.get_or_run(key.task_key(), work)
    }

    /// Resolves `key` in the view as a job of [`Cx::get_all`]'s, in a task of its own, ahead of
    /// the task that asked, which asks it again in its turn: whatever stops this run (a key
    /// another task runs, a cancel, a file it can't read) leaves the key for that task.
    fn ahead<Q: Query>(&self, state: &ViewState, key: &Q::Key) {
        let task = blaze_reclaim::enter();
        let query = QueryKey { kind: Q::KIND, input: Q::input(key) };
        if state.results.get(&task, &query).is_some() {
            return;
        }
        self.learn_key::<Q>(&task, query, key);
        if self.resolve(&task, state, query, Run::ahead(), Some(key)).is_err() {
            // Not this job's to report: the asker meets the same file and fails with it.
            UNREADABLE.with(Cell::take);
        }
    }

    /// [`Database::resolve`] while holding `key`.
    fn resolve_held(
        &self,
        task: &Guard,
        state: &ViewState,
        info: &KindInfo,
        key: QueryKey,
        run: Run,
        typed: Option<&(dyn Any + Send + Sync)>, // dyn: see `KeySource::Typed`
    ) -> Result<Option<MemoId>, Cancelled> {
        if let Some(resolved) = state.results.get(task, &key) {
            return Ok(Some(resolved.memo));
        }
        let _frame = Frame::push(key);
        let slot = self.slot(task, &key);
        let mut previous = None;
        for &memo in slot.map_or(&[][..], Slot::memos).iter().rev() {
            if self.store.hot(task, memo).code() != info.code {
                continue;
            }
            let change = match self.verify(task, state, memo, run)? {
                Verdict::Valid => return Ok(Some(self.resolved(task, state, key, memo))),
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
        Ok(Some(self.resolved(task, state, key, memo)))
    }

    /// Records `memo` as `key`'s in the view, and says which memo the view has for it (a
    /// racing run's, if it recorded first).
    fn resolved(&self, task: &Guard, state: &ViewState, key: QueryKey, memo: MemoId) -> MemoId {
        state.results.insert(task, key, Resolved::new(&self.store, task, memo)).memo
    }

    /// Whether every edge of `memo`, in order, still holds in the view: one sequential walk of
    /// its range of the edge segment, each edge's key and fingerprint read in place. As it
    /// checks one edge it prefetches the place of the edge `AHEAD` on and the row of the edge
    /// `AHEAD / 2` on, so their misses overlap; it still stops at the first edge that changed.
    /// A database small enough to stay in cache (`PREFETCH_FROM`) skips the prefetches.
    fn verify(&self, task: &Guard, state: &ViewState, memo: MemoId, run: Run) -> Result<Verdict, Cancelled> {
        let edges = self.store.edges(task, memo);
        let far = self.store.end() >= PREFETCH_FROM;
        for (at, edge) in edges.iter().enumerate() {
            if far && let Some(ahead) = edges.get(at + AHEAD) {
                let read = ahead.read();
                if is_input(read.kind) {
                    state.inputs.prefetch_place(task, &read);
                } else {
                    state.results.prefetch_place(task, &read);
                }
            }
            if far && let Some(ahead) = edges.get(at + AHEAD / 2) {
                let read = ahead.read();
                if is_input(read.kind) {
                    state.inputs.prefetch_entry(task, &read);
                } else {
                    state.results.prefetch_entry(task, &read);
                }
            }
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
        let computed = blaze_mem::frame::task(info.tag, |_| (info.run)(&mut cx, source))?;
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
        typed: &(dyn Any + Send + Sync), // dyn: see `KeySource::Typed`
    ) -> Result<&'g [u8], Cancelled> {
        loop {
            if let Some(bytes) = self.store.bytes(task, memo) {
                return Ok(bytes);
            }
            let Some(info) = self.kinds.get(key.kind) else {
                unreachable!("a memo of kind {} was made, but the kind is not registered", key.kind.0)
            };
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
            state.results.get(task, &key).map(Resolved::fingerprint)
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
        // alloc: one pack, its entries and its name a collection that moves values.
        let mut bytes = Vec::new();
        let mut entries = Vec::new(); // alloc: see above
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
        let file = FileName::new(&format!("{dir}/cold/{}", io.cold_files)) // alloc: see above
            .expect("a name under the database's directory");
        io.cold_files += 1;
        let tag = io.tag(self.config.tags);
        effects.push(Effect::Write { file: file.clone(), bytes, tag }); // clone: see above
        io.pending.insert(tag, Pending::ColdWrite { file, entries });
    }

    fn image_entries<'g>(&'g self, task: &'g Guard) -> image::Entries<'g> {
        // One list for every key, one for every memo, and one sort buffer reused across keys.
        // alloc: once per image, as are the lists below.
        let mut keys: Vec<QueryKey> = Vec::new();
        self.slots.for_each(task, |key, _| keys.push(*key));
        if let Some(image) = &self.image {
            image.for_each_key(|key| keys.push(key));
        }
        keys.sort_unstable();
        keys.dedup();
        // Reading a key's memos in from the image may add codes, so a code past the snapshot
        // takes a new one.
        let mut codes = self.store.codes();
        let mut entries = image::Entries { keys: Vec::with_capacity(keys.len()), memos: Vec::with_capacity(keys.len()) };
        let mut sorting: Vec<(Fingerprint, ContentId, SchemaHash, &[EdgeRecord], MemoId)> = Vec::new(); // alloc: see above
        for key in keys {
            let Some(slot) = self.slot(task, &key) else { continue };
            if slot.memos().is_empty() {
                continue;
            }
            sorting.clear();
            for &memo in slot.memos() {
                let hot = self.store.hot(task, memo);
                if hot.code() as usize >= codes.len() {
                    codes = self.store.codes();
                }
                let (schema, producer) = codes[hot.code() as usize];
                sorting.push((hot.fingerprint(), producer, schema, self.store.edges(task, memo), memo));
            }
            sorting.sort_by(|a, b| {
                (a.0, a.1, a.2, a.3.len())
                    .cmp(&(b.0, b.1, b.2, b.3.len()))
                    .then_with(|| a.3.iter().map(EdgeRecord::order).cmp(b.3.iter().map(EdgeRecord::order)))
            });
            let start = entries.memos.len();
            entries.memos.extend(sorting.iter().map(|&(.., memo)| memo));
            entries.keys.push(image::Entry { key, memos: start..entries.memos.len(), key_bytes: slot.key() });
        }
        entries
    }

    fn image_inputs(&self, task: &Guard) -> Vec<(QueryKey, Input)> {
        let mut inputs = self.inputs.snapshot(task);
        inputs.sort_by_key(|(key, _)| *key);
        inputs
    }
}
