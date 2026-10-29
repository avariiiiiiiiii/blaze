//! blaze v2's store (docs/v2/store.md section 8). Replaces `blaze-store`.
//!
//! - A value is kept under a [`Key`]: the BLAKE3 hash of exactly what it was made from (the
//!   source, cfg, features, target, toolchain, the layout facts read, the schema and code of
//!   what made it). A key that misses an input is a bug.
//! - Values live in **packs**: aligned zero-copy records, mmapped and read in place; hot ones
//!   uncompressed, cold ones zstd with a dictionary per [`Kind`]; large outputs chunked by
//!   content so near-identical binaries share bytes. A pack is appended to and its index
//!   written into the inactive of two slots, then the header flipped: one barrier, not a sync
//!   per entry (#1298).
//! - The **index** is an mmapped open-addressed table on keys: one or two cache misses a
//!   lookup.
//! - **Verified**: [`Store::get`] checks a value's content hash before handing it out; a value
//!   that fails is [`Corrupt`], never served. Paths of archives and outputs can't escape their
//!   directory.
//! - **Only the daemon writes the store**: user code's outputs are hashed and checked by the
//!   daemon before they are put. Each tenant has a store; only the read-only base layer of
//!   public dependencies is shared.
//! - [`Store`] does no IO: it maps and appends through [`blaze_core::Effect`]s.

use std::fmt;

use blaze_core::{Completion, Effect, FileName, Mapping, Tag};
use blaze_ids::ContentId;
use serde::{Deserialize, Serialize};

/// What a value was made from, hashed.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Key(pub ContentId);

/// What kind of value: picks its compression dictionary and its pack.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Kind {
    /// A query's result in the zero-copy format.
    Query,
    /// A compiled function's blob (`blaze-codegen`).
    Code,
    /// Debug info and panic locations of a compiled function, by its key.
    Debug,
    /// A build script's or proc macro's run: its outputs and traced inputs.
    Run,
    /// A whole output (an executable, a library), as content-defined chunks.
    Output,
    /// The daemon's image.
    Image,
}

/// A value as the store holds it, read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry<'a> {
    pub key: Key,
    pub kind: Kind,
    /// The BLAKE3 hash of `bytes`, checked.
    pub content: ContentId,
    pub bytes: &'a [u8],
}

/// A put's place in line for durability.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Ticket(pub u64);

/// A value whose bytes are not what the store recorded.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Corrupt {
    pub key: Key,
    pub pack: FileName,
    pub what: String,
}

impl fmt::Display for Corrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the value of {:?} in {} is corrupt: {}", self.key.0, self.pack, self.what)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Config {
    /// The store's directory, under the driver's root.
    pub dir: FileName,
    /// Read-only stores under it, searched after it: the shared base layer.
    pub layers: Vec<FileName>,
    /// Tags it asks with are `tags..tags + 2^32`.
    pub tags: u64,
}

/// One store, as a state machine.
pub struct Store {
    _private: (),
}

impl Store {
    /// A store that is opening: it asks to map its index and packs.
    pub fn open(config: Config, effects: &mut Vec<Effect>) -> Store {
        let _ = (config, effects);
        unimplemented!("v2: opening the store (v2-store)")
    }

    /// Whether it has mapped what it needs to answer gets.
    pub fn ready(&self) -> bool {
        unimplemented!("v2: whether the store is open (v2-store)")
    }

    /// The value of `key`, verified; `None` when it has none.
    pub fn get(&self, key: &Key) -> Result<Option<Entry<'_>>, Corrupt> {
        let _ = key;
        unimplemented!("v2: a verified get from the mapped index and packs (v2-store)")
    }

    /// Whether it has a value of `key`, unverified: for planning, never for use.
    pub fn has(&self, key: &Key) -> bool {
        let _ = key;
        unimplemented!("v2: a lookup in the index (v2-store)")
    }

    /// Keeps `bytes` under `key`; readable at once, durable once [`Store::complete`] says
    /// its ticket is. A second put of a key keeps the first value, and a put whose bytes differ
    /// from the value kept is an error the caller sees (a key that misses an input).
    pub fn put(&mut self, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<Ticket, Mismatch> {
        let _ = (key, kind, bytes);
        unimplemented!("v2: a put into the open pack (v2-store)")
    }

    /// Asks for what puts made since the last flush need to be durable.
    pub fn flush(&mut self, effects: &mut Vec<Effect>) {
        let _ = effects;
        unimplemented!("v2: group commit of the packs and the index (v2-store)")
    }

    /// Checks every value of `kind` (all, for `None`): what a scrub or the oracle runs.
    pub fn verify(&self, kind: Option<Kind>) -> Vec<Corrupt> {
        let _ = kind;
        unimplemented!("v2: verifying the store (v2-store)")
    }

    /// Asks for the value of `key` to be placed at `to`, sharing its blocks (reflink,
    /// `clonefile`); an output split in chunks is written whole.
    pub fn place(&mut self, key: &Key, to: FileName, effects: &mut Vec<Effect>) -> Option<Tag> {
        let _ = (key, to, effects);
        unimplemented!("v2: placing a value as a file (v2-store)")
    }

    pub fn owns(&self, tag: Tag) -> bool {
        let _ = tag;
        unimplemented!("v2: the store's tags (v2-store)")
    }

    /// Takes a completion of its own: a mapping, an append, a sync. Says which puts are now
    /// durable.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        let _ = (completion, effects);
        unimplemented!("v2: the store's completions (v2-store)")
    }

    /// Takes a mapping it asked for. Separate from [`Store::complete`] for drivers that map
    /// by themselves at start.
    pub fn mapped(&mut self, file: FileName, mapping: Mapping) {
        let _ = (file, mapping);
        unimplemented!("v2: a mapped pack or index (v2-store)")
    }
}

/// A put of a key the store has with other bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mismatch {
    pub key: Key,
    pub kept: ContentId,
    pub given: ContentId,
}

/// The remote tier, for machines outside the pool (a laptop): read-through, never on the
/// VM's hot path. A value from it is verified as any other before it is kept.
pub trait Remote {
    /// Asks for the value of `key`; the answer comes as a completion of `tag`.
    fn fetch(&mut self, key: &Key, tag: Tag, effects: &mut Vec<Effect>);
}
