//! blaze v2's query database (docs/v2/db.md section 7, docs/v2/items.md section 6.2), remade
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
//! - A **fact** is the narrowest thing a query read (docs/v2/items.md section 6.2's table): a
//!   query of its own whose value is small and whose fingerprint is the hash it stands for (an
//!   interface hash, a layout hash, "N in S resolves to X"). The kinds of fact are
//!   `blaze-items`'s.
//! - The only per-view queries are the inputs ([`Input`]): a file of the view, the view's
//!   configuration. Everything else is a function of content.
//! - [`Why`] answers, for a query that ran, the chain of edges back to the input that changed.

use std::fmt;

use blaze_core::{Completion, Effect, Mapping};
use blaze_format::{Persist, SchemaHash};
use blaze_ids::{ContentId, Fingerprint};
use blaze_vfs::{View, ViewId};
use serde::{Deserialize, Serialize};

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

impl Input {
    pub fn key(&self) -> QueryKey {
        unimplemented!("v2: the query key of an input (v2-db)")
    }
}

/// A query: a pure function of its key, read through [`Cx`], which records every edge.
pub trait Query: 'static {
    const KIND: QueryKind;
    const NAME: &'static str;
    /// The content ID of the code of the query's function, for [`Memo::producer`]; set by the
    /// build (blaze's own item hashes of it).
    const PRODUCER: ContentId;
    type Key: Clone + Send + Sync;
    type Value: Persist;
    /// The content ID of `key`, for [`QueryKey::input`].
    fn input(key: &Self::Key) -> ContentId;
    fn execute(cx: &mut Cx<'_>, key: &Self::Key) -> Result<Self::Value, Cancelled>;
    /// The value's fingerprint: by default the hash of its bytes in the format.
    fn fingerprint(value: &Self::Value) -> Fingerprint {
        let _ = value;
        unimplemented!("v2: a value's fingerprint from its bytes (v2-db)")
    }
}

/// A task's view of the database while it runs a query: everything read through it is an
/// edge of the result.
pub struct Cx<'db> {
    db: &'db Database,
    view: ViewId,
}

impl<'db> Cx<'db> {
    pub fn view(&self) -> ViewId {
        self.view
    }

    pub fn database(&self) -> &'db Database {
        self.db
    }

    /// Another query's value: reused, waited on if running, or run inline. Records the edge.
    pub fn get<Q: Query>(
        &mut self,
        key: &Q::Key,
    ) -> Result<<Q::Value as Persist>::Ref<'db>, Cancelled> {
        let _ = key;
        unimplemented!("v2: asking a query from a query (v2-db)")
    }

    /// A file of the view: its content ID and bytes; `None` for none. Records the edge.
    pub fn file(&mut self, path: &str) -> Result<Option<blaze_vfs::File<'db>>, Cancelled> {
        let _ = path;
        unimplemented!("v2: reading an input file (v2-db)")
    }

    /// Records that the query read `fact`, whose fingerprint was `saw`: for facts computed
    /// without asking a query (an index the caller holds).
    pub fn read(&mut self, fact: QueryKey, saw: Fingerprint) {
        let _ = (fact, saw);
        unimplemented!("v2: recording a fact read (v2-db)")
    }
}

/// The query was cancelled: a nested request needs what an outer one holds (#1301), or its
/// request ended. It is reposted, never answered with a partial value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cancelled;

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

/// The one database of a daemon: every view's results, the memo of each key.
pub struct Database {
    _private: (),
}

impl Database {
    pub fn empty() -> Database {
        unimplemented!("v2: an empty database (v2-db)")
    }

    /// The database in a mapped image, used in place.
    pub fn from_image(image: Mapping) -> Result<Database, DbError> {
        let _ = image;
        unimplemented!("v2: the database from its image (v2-db)")
    }

    pub fn register<Q: Query>(&mut self) {
        unimplemented!("v2: registering a query kind (v2-db)")
    }

    pub fn open(&self, view: View) -> ViewId {
        let _ = view;
        unimplemented!("v2: a view of the database (v2-db)")
    }

    pub fn close(&self, view: ViewId) {
        let _ = view;
        unimplemented!("v2: closing a view (v2-db)")
    }

    /// A query's value in `view`, from outside any query (a request's root).
    pub fn get<Q: Query>(
        &self,
        view: ViewId,
        key: &Q::Key,
    ) -> Result<<Q::Value as Persist>::Ref<'_>, DbError> {
        let _ = (view, key);
        unimplemented!("v2: asking a query (v2-db)")
    }

    pub fn memo(&self, key: &QueryKey) -> Option<Memo> {
        let _ = key;
        unimplemented!("v2: a key's memo (v2-db)")
    }

    pub fn why(&self, view: ViewId, key: &QueryKey) -> Option<Why> {
        let _ = (view, key);
        unimplemented!("v2: why a query reran (v2-db)")
    }

    /// Drops what no open view and no key of `kept` reaches; moves cold values to the store.
    pub fn collect(&self, kept: &[QueryKey], effects: &mut Vec<Effect>) {
        let _ = (kept, effects);
        unimplemented!("v2: garbage collection by reachability (v2-db)")
    }

    /// Writes the whole database as one image, through effects.
    pub fn write_image(&self, effects: &mut Vec<Effect>) {
        let _ = effects;
        unimplemented!("v2: writing the daemon image (v2-db)")
    }

    pub fn complete(&self, completion: Completion, effects: &mut Vec<Effect>) {
        let _ = (completion, effects);
        unimplemented!("v2: the database's completions (v2-db)")
    }
}
