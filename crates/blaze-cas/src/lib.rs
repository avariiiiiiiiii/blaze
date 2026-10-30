//! blaze v2's store (docs/v2/plan/lanes/store.md section 8). Replaces `blaze-store`.
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
//!
//! The layout on disk and why it survives a crash at any point: docs/v2-store.md.

mod chunk;
mod cold;
mod copy;
mod crc;
mod disk;
pub mod remote;
#[cfg(test)]
mod tests;

use std::{
    borrow::Cow,
    fmt,
    sync::{Mutex, MutexGuard, PoisonError},
};

use blaze_core::{Completion, Done, Effect, FileName, Mapping, Tag};
use blaze_format::SchemaHash;
use blaze_ids::{ContentId, Hasher};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

use crate::disk::{
    CHUNK_DOMAIN, DICTIONARY_DOMAIN, DictionaryRef, ExternalRef, Form, HEAD_FILE, Head,
    IndexRef, Loc, NO_DICTIONARY, PACK_HEADER, PackRef, RECORD_HEADER, RecordHeader, Table,
    content_of,
};

/// The format of everything in a store's directory. A store of another refuses to open, naming
/// both; nothing reads it as this one.
pub(crate) const FORMAT: u32 = 2;

/// A pack is closed for appends once it is this long; a record longer than that has a pack of
/// its own.
#[cfg(not(test))]
const PACK_MOST: u64 = 1 << 30;
#[cfg(test)]
const PACK_MOST: u64 = 256 << 10;

/// Outputs from this long on are kept as content-defined chunks.
const CHUNK_FROM: usize = 256 << 10;

/// Hot values (not outputs) from this long on are files of their own, so they are placed by
/// sharing blocks and mapped on their own.
#[cfg(not(test))]
const EXTERNAL_FROM: usize = 4 << 20;
#[cfg(test)]
const EXTERNAL_FROM: usize = 96 << 10;

/// A new index table is written once the records not in the mapped one reach this many.
#[cfg(not(test))]
const INDEX_AFTER: usize = 1 << 16;
#[cfg(test)]
const INDEX_AFTER: usize = 48;

/// IO that fails this many times in a row breaks the store.
const FAILURES_MOST: u32 = 8;

const TAGS: u64 = 1 << 32;

/// The packs of one store; a record's place in memory is its tier, pack and offset in 64 bits.
const PACKS_MOST: usize = 1 << 16;

/// What a value was made from, hashed.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Key(pub ContentId);

impl Key {
    /// The key of what `producer` makes from `inputs` (#1439): `schema` is the schema of the
    /// value made, `producer` the content ID of the code that made it (the code in its own
    /// dependency closure, never a hash of all of blaze's sources), `inputs` the content ID of
    /// every input it read, in the order it read them. An input left out is a bug: the value
    /// would be reused for inputs it was not made from.
    pub fn of(schema: SchemaHash, producer: ContentId, inputs: &[ContentId]) -> Key {
        let mut hasher = Hasher::new("blaze v2 store key 1");
        hasher.id(&schema.0).id(&producer).update(&(inputs.len() as u64).to_le_bytes());
        for input in inputs {
            hasher.id(input);
        }
        Key(hasher.finish())
    }
}

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

impl Kind {
    pub const ALL: [Kind; 6] =
        [Kind::Query, Kind::Code, Kind::Debug, Kind::Run, Kind::Output, Kind::Image];

    /// The kind's byte on disk and on the wire.
    pub const fn code(self) -> u8 {
        match self {
            Kind::Query => 0,
            Kind::Code => 1,
            Kind::Debug => 2,
            Kind::Run => 3,
            Kind::Output => 4,
            Kind::Image => 5,
        }
    }

    pub const fn from_code(code: u8) -> Option<Kind> {
        match code {
            0 => Some(Kind::Query),
            1 => Some(Kind::Code),
            2 => Some(Kind::Debug),
            3 => Some(Kind::Run),
            4 => Some(Kind::Output),
            5 => Some(Kind::Image),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Kind::Query => "query",
            Kind::Code => "code",
            Kind::Debug => "debug",
            Kind::Run => "run",
            Kind::Output => "output",
            Kind::Image => "image",
        }
    }

    /// Read rarely (debug info on a panic or in a debugger; a run's outputs once per hit), so
    /// kept compressed. The rest are read in place on every warm path.
    const fn cold(self) -> bool {
        matches!(self, Kind::Debug | Kind::Run)
    }
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

/// Why a store stopped: it could not open, or its IO failed [`FAILURES_MOST`] times in a row.
/// What it has mapped it still serves; nothing it is given is made durable.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Broken {
    pub file: FileName,
    pub what: String,
}

impl fmt::Display for Broken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the store cannot go on: {}: {}", self.file, self.what)
    }
}

/// How a [`Store::fetch`] from the remote tier ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Fetched {
    /// Verified and kept, durable once its ticket is.
    Kept(Ticket),
    /// The remote has no value of the key.
    Missing,
    /// Not kept: the remote could not be reached, or sent what is not the value it named.
    Refused(String),
}

/// How a [`Store::place`] ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Placed {
    /// The tag `place` returned.
    pub tag: Tag,
    pub to: FileName,
    pub result: Result<(), String>,
}

/// One store, as a state machine.
///
/// Only the daemon holds a store, and one daemon a directory (#686): it is the one writer of its
/// packs and its head, and every build's reads go through its one mapped index.
pub struct Store {
    config: Config,
    /// Its own directory first, then the layers in order.
    tiers: Vec<Tier>,
    tags: Tags,
    broken: Option<Broken>,
    /// IO failures in a row.
    failures: u32,
    next_ticket: u64,
    /// Every ticket up to this one is durable.
    durable: u64,
    commit: Option<Commit>,
    /// A flush asked for while a commit was in flight.
    flush_wanted: bool,
    /// Counts the records put, so a new index table knows which of them it holds.
    seq: u64,
    /// A new index table was written and its mapping has not come.
    index_mapping: bool,
    training: [Training; Kind::ALL.len()],
    problems: Vec<String>,
    fetching: FxHashMap<Key, Tag>,
    fetched: Vec<(Key, Fetched)>,
    placed: Vec<Placed>,
    /// Records whose bytes have been hashed and found to be what was recorded. A mapping and the
    /// memory of a pack never change the bytes at a place, so once is enough.
    verified: Mutex<FxHashSet<u64>>,
    /// Values made whole to be read (decompressed, or put together from chunks), by place.
    owned: Mutex<FxHashMap<u64, (ContentId, Box<[u8]>)>>,
}

/// The tags a store asks with, and what each asked for.
struct Tags {
    base: u64,
    next: u64,
    ops: FxHashMap<Tag, Op>,
}

impl Tags {
    fn fresh(&mut self) -> Tag {
        let tag = Tag(self.base.wrapping_add(self.next));
        self.next = (self.next + 1) % TAGS;
        tag
    }

    fn ask(&mut self, op: Op) -> Tag {
        let tag = self.fresh();
        self.ops.insert(tag, op);
        tag
    }
}

enum Op {
    Head { tier: usize },
    Map { tier: usize, file: FileName, what: Mapped },
    Truncate { pack: usize },
    Append { pack: usize },
    Sync { pack: usize },
    WriteExternal { content: ContentId },
    WriteIndex,
    WriteHead,
    /// `to` removed so `from` can be cloned there.
    Clear { asked: Tag, from: FileName, to: FileName },
    Place { asked: Tag, to: FileName },
    Fetch { key: Key },
}

#[derive(Clone, Copy, Debug)]
enum Mapped {
    /// `seq` is `None` for the table the store opened with, else the last record it holds.
    Index { seq: Option<u64> },
    /// `usable` is how much of the file is the store's as of the commit that asked.
    Pack { pack: usize, usable: u64 },
    External { content: ContentId },
}

impl Mapped {
    fn usable(&self) -> u64 {
        match self {
            Mapped::Pack { usable, .. } => *usable,
            _ => 0,
        }
    }
}

/// A store's directory as it was read and as it is being written.
struct Tier {
    dir: FileName,
    /// The head as last read or durably written.
    head: Head,
    head_read: bool,
    opened: bool,
    /// Mappings of the opening still to come.
    waiting: usize,
    index: Option<Mapping>,
    packs: Vec<Pack>,
    /// Records the index table does not hold: read from the packs' tails at opening, or put
    /// since. The number is the record's `seq`.
    delta: FxHashMap<Key, (Loc, u64)>,
    externals: FxHashMap<ContentId, External>,
    /// By number, which a compressed record names.
    dictionaries: Vec<Dictionary>,
}

impl Tier {
    fn new(dir: FileName) -> Tier {
        Tier {
            dir,
            head: Head::default(),
            head_read: false,
            opened: false,
            waiting: 0,
            index: None,
            packs: Vec::new(),
            delta: FxHashMap::default(),
            externals: FxHashMap::default(),
            dictionaries: Vec::new(),
        }
    }

    fn file(&self, name: &str) -> FileName {
        FileName::new(&format!("{}/{name}", self.dir))
            .expect("a store's file names stay in its directory")
    }

    fn index_file(&self) -> FileName {
        self.file(&disk::index_file(self.head.index.map_or(0, |it| it.slot)))
    }

    fn table(&self) -> Option<Table<'_>> {
        self.index.as_ref().and_then(|it| Table::new(it.bytes()).ok())
    }

    fn find(&self, key: &Key) -> Option<Result<Loc, &'static str>> {
        if let Some((loc, _)) = self.delta.get(key) {
            return Some(Ok(*loc));
        }
        self.table()?.get(key)
    }
}

struct Pack {
    kind: Kind,
    number: u32,
    file: FileName,
    mapping: Option<Mapping>,
    /// How much of `mapping` is the store's.
    usable: u64,
    /// The bytes from `usable` to `end`, held until a mapping covers them.
    memory: Vec<u8>,
    /// What the head on disk says is the store's.
    committed: u64,
    indexed: u64,
    end: u64,
}

impl Pack {
    fn bytes(&self, at: u64, len: u64) -> Option<&[u8]> {
        let end = at.checked_add(len)?;
        if end <= self.usable {
            let bytes = self.mapping.as_ref()?.bytes();
            bytes.get(usize::try_from(at).ok()?..usize::try_from(end).ok()?)
        } else if at >= self.usable && end <= self.end {
            self.memory.get((at - self.usable) as usize..(end - self.usable) as usize)
        } else {
            None
        }
    }
}

struct External {
    len: u64,
    file: FileName,
    mapping: Option<Mapping>,
    /// Held until it is written and mapped.
    memory: Option<Vec<u8>>,
    committed: bool,
}

struct Dictionary {
    kind: Kind,
    pack: u32,
    offset: u64,
    bytes: Result<Vec<u8>, String>,
}

/// The raw values of a cold kind kept so far, to train its dictionary on.
#[derive(Default)]
struct Training {
    samples: Vec<(u32, u64, u64)>,
    bytes: u64,
    /// No training before this many samples: doubled after one fails.
    after: usize,
}

/// One group commit: each pack cut to what is committed, appended to, synced; new externals
/// and a new index table written; then the head.
struct Commit {
    /// Each pack written and the end it is written to.
    packs: Vec<(usize, u64)>,
    externals: Vec<ContentId>,
    /// The slot of the index table it wrote, and the last record that table holds.
    index: Option<(u8, u64)>,
    head: Head,
    /// The last ticket it makes durable.
    tickets: u64,
    steps: usize,
    failed: Option<Broken>,
    head_sent: bool,
}

impl Commit {
    fn to(&self, pack: usize) -> Option<u64> {
        self.packs.iter().find(|(it, _)| *it == pack).map(|(_, to)| *to)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A record's place in the store, as one number.
fn location(tier: usize, pack: u32, offset: u64) -> u64 {
    ((tier as u64) << 56) | (u64::from(pack) << 40) | offset
}

fn describe(done: &Done) -> String {
    match done {
        Done::Failed(error) => error.0.clone(),
        Done::Read(_) => "it was answered with a read".into(),
        Done::Mapped(_) => "it was answered with a mapping".into(),
        _ => "it was answered as another kind of effect".into(),
    }
}

impl Store {
    /// A store that is opening: it asks to map its index and packs.
    pub fn open(config: Config, effects: &mut Vec<Effect>) -> Store {
        let mut tiers = vec![Tier::new(config.dir.clone())];
        tiers.extend(config.layers.iter().cloned().map(Tier::new));
        let mut store = Store {
            tags: Tags { base: config.tags, next: 0, ops: FxHashMap::default() },
            config,
            tiers,
            broken: None,
            failures: 0,
            next_ticket: 1,
            durable: 0,
            commit: None,
            flush_wanted: false,
            seq: 0,
            index_mapping: false,
            training: Default::default(),
            problems: Vec::new(),
            fetching: FxHashMap::default(),
            fetched: Vec::new(),
            placed: Vec::new(),
            verified: Mutex::default(),
            owned: Mutex::default(),
        };
        for tier in 0..store.tiers.len() {
            let file = store.tiers[tier].file(HEAD_FILE);
            let tag = store.tags.ask(Op::Head { tier });
            effects.push(Effect::ReadAll { file, tag });
        }
        store
    }

    /// Whether it has mapped what it needs to answer gets.
    pub fn ready(&self) -> bool {
        self.broken.is_none() && self.tiers.iter().all(|it| it.opened)
    }

    /// Why it stopped, if it has.
    pub fn broken(&self) -> Option<&Broken> {
        self.broken.as_ref()
    }

    /// What it could not do and went on without: a dictionary it could not train.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// Whether the put that got `ticket` is durable.
    pub fn durable(&self, ticket: Ticket) -> bool {
        ticket.0 <= self.durable
    }

    /// The value of `key`, verified; `None` when it has none.
    pub fn get(&self, key: &Key) -> Result<Option<Entry<'_>>, Corrupt> {
        let Some(found) = self.find(key) else { return Ok(None) };
        let (tier, loc) = found?;
        self.read(tier, key, loc).map(Some)
    }

    /// Whether it has a value of `key`, unverified: for planning, never for use.
    pub fn has(&self, key: &Key) -> bool {
        self.find(key).is_some()
    }

    /// Keeps `bytes` under `key`; readable at once, durable once [`Store::complete`] says
    /// its ticket is. A second put of a key keeps the first value, and a put whose bytes differ
    /// from the value kept is an error the caller sees (a key that misses an input).
    pub fn put(&mut self, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<Ticket, Mismatch> {
        assert!(self.tiers[0].opened, "a put into a store that is not open");
        self.forget_owned();
        let content = content_of(&bytes);
        if let Some((tier, loc)) = self.find_kept(&key) {
            if let Some(kept) = self.content_at(tier, &key, loc) {
                if kept != content {
                    return Err(Mismatch { key, kept, given: content });
                }
                return Ok(self.ticket_of_kept(tier, loc));
            }
            // A record whose header is corrupt is shadowed by the new one.
        }
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        let raw = bytes.len() as u64;
        if kind == Kind::Output && bytes.len() >= CHUNK_FROM {
            let mut refs = Vec::new();
            for range in chunk::chunks(&bytes) {
                let piece = &bytes[range];
                let chunk = Key(ContentId::of(CHUNK_DOMAIN, piece));
                if self.find_kept(&chunk).is_none() {
                    let len = piece.len() as u64;
                    match cold::lz4(piece) {
                        Some(stored) => {
                            let (kind, content) = (Kind::Output, content_of(piece));
                            self.append_value(chunk, kind, Form::Lz4, 0, content, len, &stored);
                        }
                        None => {
                            self.append(chunk, Kind::Output, Form::Raw, 0, content_of(piece), piece);
                        }
                    }
                }
                refs.push((chunk, piece.len() as u64));
            }
            let payload = disk::encode_chunks(&refs);
            self.append_value(key, kind, Form::Chunked, 0, content, raw, &payload);
        } else if !kind.cold() && kind != Kind::Output && bytes.len() >= EXTERNAL_FROM {
            let own = &mut self.tiers[0];
            let file = own.file(&disk::external_file(&content));
            own.externals.entry(content).or_insert(External {
                len: raw,
                file,
                mapping: None,
                memory: Some(bytes),
                committed: false,
            });
            self.append_value(key, kind, Form::External, 0, content, raw, &[]);
        } else if kind.cold() && bytes.len() >= cold::LONG_FROM {
            match cold::compress_long(&bytes) {
                Ok(stored) => {
                    self.append_value(key, kind, Form::Zstd, NO_DICTIONARY, content, raw, &stored);
                }
                Err(why) => self.cannot_compress(kind, why),
            }
        } else if kind.cold() {
            match self.dictionary_of(kind) {
                Some(number) => {
                    let dictionary = self.tiers[0].dictionaries[number as usize]
                        .bytes
                        .as_ref()
                        .expect("a dictionary of its own is held");
                    match cold::compress(dictionary, &bytes) {
                        Ok(stored) => {
                            self.append_value(key, kind, Form::Zstd, number, content, raw, &stored);
                        }
                        Err(why) => self.cannot_compress(kind, why),
                    }
                }
                None => {
                    let loc = self.append(key, kind, Form::Raw, 0, content, &bytes);
                    self.sample(kind, loc);
                }
            }
        } else {
            self.append(key, kind, Form::Raw, 0, content, &bytes);
        }
        Ok(ticket)
    }

    /// Asks for what puts made since the last flush need to be durable.
    pub fn flush(&mut self, effects: &mut Vec<Effect>) {
        assert!(self.tiers[0].opened, "a flush of a store that is not open");
        self.forget_owned();
        if self.broken.is_some() {
            return;
        }
        if self.commit.is_some() {
            self.flush_wanted = true;
        } else {
            self.start_commit(effects);
        }
    }

    /// Checks every value of `kind` (all, for `None`): what a scrub or the oracle runs.
    pub fn verify(&self, kind: Option<Kind>) -> Vec<Corrupt> {
        let mut out = Vec::new();
        let nothing = Key(ContentId::default());
        for (tier_at, tier) in self.tiers.iter().enumerate() {
            if let (None, Some(index), Some(mapping)) = (kind, tier.head.index, &tier.index) {
                let found = crc::crc32c(mapping.bytes());
                if found != index.check {
                    out.push(Corrupt {
                        key: nothing,
                        pack: tier.index_file(),
                        what: format!(
                            "the index's CRC-32C is {found:08x}, not the {:08x} its head names",
                            index.check
                        ),
                    });
                }
            }
            for dictionary in &tier.dictionaries {
                if let (true, Err(why)) =
                    (kind.is_none_or(|it| it == dictionary.kind), &dictionary.bytes)
                {
                    out.push(Corrupt {
                        key: nothing,
                        pack: tier.packs[dictionary.pack as usize].file.clone(),
                        what: format!("its {} dictionary {why}", dictionary.kind.name()),
                    });
                }
            }
            let mut entries = Vec::new();
            if let Some(table) = tier.table() {
                for (key, loc) in table.iter() {
                    match loc {
                        Ok(_) if tier.delta.contains_key(&key) => {}
                        Ok(loc) => entries.push((key, loc)),
                        Err(what) => {
                            out.push(Corrupt { key, pack: tier.index_file(), what: what.into() })
                        }
                    }
                }
            }
            entries.extend(tier.delta.iter().map(|(key, (loc, _))| (*key, *loc)));
            entries.sort_unstable_by_key(|(key, loc)| (loc.pack, loc.offset, *key));
            for (key, loc) in entries {
                if kind.is_some_and(|it| it != loc.kind) {
                    continue;
                }
                if let Err(corrupt) = self.value(tier_at, &key, loc, false) {
                    out.push(corrupt);
                }
            }
        }
        out
    }

    /// Asks for the value of `key` to be placed at `to`, sharing its blocks (reflink,
    /// `clonefile`); an output split in chunks is written whole.
    ///
    /// How it went comes through [`Store::placed`]. What a value's form decides is how it is
    /// placed, never a try of one way then another: a file of its own is cloned (and fails
    /// where the file system cannot share blocks), anything else written.
    pub fn place(&mut self, key: &Key, to: FileName, effects: &mut Vec<Effect>) -> Option<Tag> {
        assert!(
            !self.tiers.iter().any(|tier| to.is_under(&tier.dir)),
            "{to} is in the store: values are placed outside it"
        );
        self.forget_owned();
        enum Plan {
            Clone(FileName),
            Write(Vec<u8>),
            Refused(String),
        }
        let found = self.find(key)?;
        let plan = match found.and_then(|(tier, loc)| Ok((tier, loc, self.read(tier, key, loc)?)))
        {
            Err(corrupt) => Plan::Refused(corrupt.to_string()),
            Ok((tier, loc, entry)) => match self.tiers[tier].externals.get(&entry.content) {
                Some(external) if loc.form == Form::External && external.committed => {
                    Plan::Clone(external.file.clone())
                }
                _ => {
                    let mut bytes = Vec::new();
                    copy::append(&mut bytes, entry.bytes);
                    Plan::Write(bytes)
                }
            },
        };
        let tag = self.tags.fresh();
        match plan {
            Plan::Clone(from) => {
                effects.push(Effect::Remove { file: to.clone(), tag });
                self.tags.ops.insert(tag, Op::Clear { asked: tag, from, to });
            }
            Plan::Write(bytes) => {
                effects.push(Effect::Write { file: to.clone(), bytes, tag });
                self.tags.ops.insert(tag, Op::Place { asked: tag, to });
            }
            Plan::Refused(why) => self.placed.push(Placed { tag, to, result: Err(why) }),
        }
        Some(tag)
    }

    /// How the places asked for since the last call went.
    pub fn placed(&mut self) -> Vec<Placed> {
        std::mem::take(&mut self.placed)
    }

    /// Asks `remote` for the value of `key`, to be verified and kept when it comes; `None` when
    /// it has a value of `key` already or is fetching it. The answer is a completion of the tag
    /// returned, handed to [`Store::complete`]; how it went comes through [`Store::fetched`].
    pub fn fetch(
        &mut self,
        key: &Key,
        remote: &mut dyn Remote,
        effects: &mut Vec<Effect>,
    ) -> Option<Tag> {
        if self.has(key) || self.fetching.contains_key(key) {
            return None;
        }
        let tag = self.tags.ask(Op::Fetch { key: *key });
        self.fetching.insert(*key, tag);
        remote.fetch(key, tag, effects);
        Some(tag)
    }

    /// How the fetches answered since the last call went.
    pub fn fetched(&mut self) -> Vec<(Key, Fetched)> {
        std::mem::take(&mut self.fetched)
    }

    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.config.tags) < TAGS
    }

    /// Takes a completion of its own: a mapping, an append, a sync. Says which puts are now
    /// durable.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        self.forget_owned();
        let Some(op) = self.tags.ops.remove(&completion.tag) else { return Vec::new() };
        let done = completion.done;
        match op {
            Op::Head { tier } => match done {
                Done::Read(bytes) => self.read_head(tier, bytes, effects),
                done => {
                    let file = self.tiers[tier].file(HEAD_FILE);
                    if self.retry(&file, &done) {
                        let tag = self.tags.ask(Op::Head { tier });
                        effects.push(Effect::ReadAll { file, tag });
                    }
                }
            },
            Op::Map { tier, file, what } => match done {
                Done::Mapped(mapping) => self.install(tier, file, what, mapping),
                done => {
                    if self.retry(&file, &done) {
                        self.ask_map(tier, file, what, effects);
                    }
                }
            },
            Op::Clear { asked, from, to } => match done {
                Done::Removed => {
                    let tag = self.tags.ask(Op::Place { asked, to: to.clone() });
                    effects.push(Effect::Clone { from, to, tag });
                }
                done => self.placed.push(Placed { tag: asked, to, result: Err(describe(&done)) }),
            },
            Op::Place { asked, to } => {
                let result = match done {
                    Done::Cloned | Done::Written => Ok(()),
                    done => Err(describe(&done)),
                };
                self.placed.push(Placed { tag: asked, to, result });
            }
            Op::Fetch { key } => {
                self.fetching.remove(&key);
                let fetched = self.take_fetched(key, done);
                self.fetched.push((key, fetched));
            }
            op => return self.step(op, done, effects),
        }
        Vec::new()
    }

    /// Takes a mapping it asked for. Separate from [`Store::complete`] for drivers that map
    /// by themselves at start.
    pub fn mapped(&mut self, file: FileName, mapping: Mapping) {
        self.forget_owned();
        let asked = self
            .tags
            .ops
            .iter()
            .filter_map(|(tag, op)| match op {
                Op::Map { file: it, what, .. } if *it == file => Some((what.usable(), *tag)),
                _ => None,
            })
            .max();
        let Some((_, tag)) = asked else { return };
        if let Some(Op::Map { tier, file, what }) = self.tags.ops.remove(&tag) {
            self.install(tier, file, what, mapping);
        }
    }

    // Reading.

    fn find(&self, key: &Key) -> Option<Result<(usize, Loc), Corrupt>> {
        for (at, tier) in self.tiers.iter().enumerate() {
            match tier.find(key) {
                Some(Ok(loc)) => return Some(Ok((at, loc))),
                Some(Err(what)) => {
                    return Some(Err(Corrupt {
                        key: *key,
                        pack: tier.index_file(),
                        what: what.into(),
                    }));
                }
                None => {}
            }
        }
        None
    }

    /// Where a value of `key` is, passing over an index slot that is not whole.
    fn find_kept(&self, key: &Key) -> Option<(usize, Loc)> {
        self.tiers
            .iter()
            .enumerate()
            .find_map(|(at, tier)| tier.find(key).and_then(Result::ok).map(|loc| (at, loc)))
    }

    /// The content its record says, unverified.
    fn content_at(&self, tier: usize, key: &Key, loc: Loc) -> Option<ContentId> {
        let pack = self.tiers[tier].packs.get(loc.pack as usize)?;
        let header = RecordHeader::decode(pack.bytes(loc.offset, RECORD_HEADER)?).ok()?;
        (header.key == *key).then_some(header.content)
    }

    fn read(&self, tier: usize, key: &Key, loc: Loc) -> Result<Entry<'_>, Corrupt> {
        let id = location(tier, loc.pack, loc.offset);
        if matches!(loc.form, Form::Zstd | Form::Chunked | Form::Lz4) {
            if let Some((content, bytes)) = self.cached(id) {
                return Ok(Entry { key: *key, kind: loc.kind, content, bytes });
            }
        }
        let (header, bytes) = self.value(tier, key, loc, true)?;
        let bytes = match bytes {
            Cow::Borrowed(bytes) => bytes,
            Cow::Owned(bytes) => self.keep(id, header.content, bytes),
        };
        Ok(Entry { key: *key, kind: header.kind, content: header.content, bytes })
    }

    /// The bytes of the record at `loc`, checked against its content hash (once a place when
    /// `memo`, every time when not).
    fn value<'a>(
        &'a self,
        tier_at: usize,
        key: &Key,
        loc: Loc,
        memo: bool,
    ) -> Result<(RecordHeader, Cow<'a, [u8]>), Corrupt> {
        let tier = &self.tiers[tier_at];
        let Some(pack) = tier.packs.get(loc.pack as usize) else {
            return Err(Corrupt {
                key: *key,
                pack: tier.index_file(),
                what: format!("its index names pack {} its head does not have", loc.pack),
            });
        };
        let corrupt = |what: String| Corrupt { key: *key, pack: pack.file.clone(), what };
        let at = loc.offset;
        let header = pack
            .bytes(at, RECORD_HEADER)
            .ok_or_else(|| corrupt(format!("its record at {at} is outside the pack")))
            .and_then(|bytes| {
                RecordHeader::decode(bytes).map_err(|what| corrupt(format!("the record at {at}: {what}")))
            })?;
        if header.key != *key {
            return Err(corrupt(format!("the record at {at} is of another key")));
        }
        let payload = pack
            .bytes(at + RECORD_HEADER, header.stored)
            .ok_or_else(|| corrupt(format!("the record at {at} runs past the pack")))?;
        let bytes: Cow<'a, [u8]> = match header.form {
            Form::Raw => Cow::Borrowed(payload),
            Form::Zstd if header.dictionary == NO_DICTIONARY => {
                Cow::Owned(cold::decompress_long(payload, header.raw).map_err(corrupt)?)
            }
            Form::Lz4 => Cow::Owned(cold::unlz4(payload, header.raw).map_err(corrupt)?),
            Form::Zstd => {
                let dictionary = tier.dictionaries.get(header.dictionary as usize).ok_or_else(
                    || corrupt(format!("it names dictionary {} the store does not have", header.dictionary)),
                )?;
                let dictionary =
                    dictionary.bytes.as_ref().map_err(|why| corrupt(format!("its dictionary {why}")))?;
                Cow::Owned(cold::decompress(dictionary, payload, header.raw).map_err(corrupt)?)
            }
            Form::Chunked => {
                let chunks = disk::decode_chunks(payload).map_err(|what| corrupt(what.into()))?;
                let total: u64 = chunks.iter().map(|(_, len)| *len).sum();
                if total != header.raw {
                    return Err(corrupt(format!(
                        "its chunks are {total} bytes, not the {} recorded",
                        header.raw
                    )));
                }
                let mut whole = Vec::with_capacity(total as usize);
                for (chunk, len) in chunks {
                    let (chunk_tier, chunk_loc) = self
                        .find(&chunk)
                        .ok_or_else(|| corrupt(format!("its chunk {} is not in the store", chunk.0)))??;
                    if !matches!(chunk_loc.form, Form::Raw | Form::Lz4) {
                        return Err(corrupt(format!("its chunk {} is not kept as a chunk", chunk.0)));
                    }
                    let (_, piece) = self.value(chunk_tier, &chunk, chunk_loc, memo)?;
                    if piece.len() as u64 != len {
                        return Err(corrupt(format!("its chunk {} is of another length", chunk.0)));
                    }
                    whole.extend_from_slice(&piece);
                }
                Cow::Owned(whole)
            }
            Form::External => {
                let Some(external) = tier.externals.get(&header.content) else {
                    return Err(corrupt(format!("its file {:x} is not in the head", header.content)));
                };
                match (&external.mapping, &external.memory) {
                    (Some(mapping), _) => Cow::Borrowed(mapping.bytes()),
                    (None, Some(bytes)) => Cow::Borrowed(bytes.as_slice()),
                    (None, None) => {
                        return Err(Corrupt {
                            key: *key,
                            pack: external.file.clone(),
                            what: "it was not mapped".into(),
                        });
                    }
                }
            }
            Form::Dictionary => {
                return Err(corrupt(format!("the record at {at} is a dictionary, not a value")));
            }
        };
        if bytes.len() as u64 != header.raw {
            return Err(corrupt(format!(
                "it is {} bytes, not the {} recorded",
                bytes.len(),
                header.raw
            )));
        }
        let id = location(tier_at, loc.pack, at);
        if !(memo && lock(&self.verified).contains(&id)) {
            let found = content_of(&bytes);
            if found != header.content {
                return Err(corrupt(format!(
                    "its bytes hash to {found:x}, not the {:x} recorded",
                    header.content
                )));
            }
            if memo {
                lock(&self.verified).insert(id);
            }
        }
        Ok((header, bytes))
    }

    fn cached(&self, id: u64) -> Option<(ContentId, &[u8])> {
        let owned = lock(&self.owned);
        let (content, bytes) = owned.get(&id)?;
        let bytes: *const [u8] = &**bytes;
        // SAFETY: as in `keep`.
        Some((*content, unsafe { &*bytes }))
    }

    fn keep(&self, id: u64, content: ContentId, bytes: Vec<u8>) -> &[u8] {
        let mut owned = lock(&self.owned);
        let (_, held) = owned.entry(id).or_insert_with(|| (content, bytes.into_boxed_slice()));
        let held: *const [u8] = &**held;
        // SAFETY: a value in `owned` is never removed, replaced or written while `self` is
        // borrowed shared: `or_insert_with` keeps the first, and only `forget_owned`, which
        // takes `&mut self`, clears the map. A rehash moves the box, not the bytes it points
        // to. So the bytes live as long as the `&self` this borrows.
        unsafe { &*held }
    }

    fn forget_owned(&mut self) {
        self.owned.get_mut().unwrap_or_else(PoisonError::into_inner).clear();
    }

    // Writing.

    fn ticket_of_kept(&mut self, tier: usize, loc: Loc) -> Ticket {
        let pack = &self.tiers[tier].packs[loc.pack as usize];
        if tier != 0 || loc.offset < pack.committed {
            return Ticket(self.durable);
        }
        if let Some(commit) = &self.commit {
            if commit.to(loc.pack as usize).is_some_and(|to| loc.offset < to) {
                return Ticket(commit.tickets);
            }
        }
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        ticket
    }

    /// The own pack of `kind` a record of `span` bytes goes into, made if none has room.
    fn writable_pack(&mut self, kind: Kind, span: u64) -> usize {
        let own = &mut self.tiers[0];
        if let Some(at) = own.packs.iter().rposition(|it| it.kind == kind) {
            let pack = &own.packs[at];
            if pack.end == PACK_HEADER || pack.end + span <= PACK_MOST {
                return at;
            }
        }
        assert!(own.packs.len() < PACKS_MOST, "the store has {PACKS_MOST} packs: it is full");
        let number = own
            .packs
            .iter()
            .filter(|it| it.kind == kind)
            .map(|it| it.number + 1)
            .max()
            .unwrap_or(0);
        let file = own.file(&disk::pack_file(kind, number));
        own.packs.push(Pack {
            kind,
            number,
            file,
            mapping: None,
            usable: 0,
            memory: disk::pack_header(kind).to_vec(),
            committed: 0,
            indexed: 0,
            end: PACK_HEADER,
        });
        own.packs.len() - 1
    }

    fn append_record(&mut self, header: RecordHeader, payload: &[u8]) -> Loc {
        let span = header.span();
        let at = self.writable_pack(header.kind, span);
        let pack = &mut self.tiers[0].packs[at];
        let offset = pack.end;
        pack.memory.extend_from_slice(&header.encode());
        pack.memory.extend_from_slice(payload);
        pack.memory.resize(pack.memory.len() + (span - RECORD_HEADER - header.stored) as usize, 0);
        pack.end += span;
        Loc {
            pack: at as u32,
            kind: header.kind,
            form: header.form,
            offset,
            stored: header.stored,
            raw: header.raw,
        }
    }

    fn append_value(
        &mut self,
        key: Key,
        kind: Kind,
        form: Form,
        dictionary: u32,
        content: ContentId,
        raw: u64,
        payload: &[u8],
    ) -> Loc {
        let header = RecordHeader {
            kind,
            form,
            dictionary,
            stored: payload.len() as u64,
            raw,
            key,
            content,
        };
        let loc = self.append_record(header, payload);
        self.seq += 1;
        self.tiers[0].delta.insert(key, (loc, self.seq));
        loc
    }

    fn append(
        &mut self,
        key: Key,
        kind: Kind,
        form: Form,
        dictionary: u32,
        content: ContentId,
        bytes: &[u8],
    ) -> Loc {
        self.append_value(key, kind, form, dictionary, content, bytes.len() as u64, bytes)
    }

    /// zstd refusing a value is a broken build of it (a missing feature), not something a
    /// value can cause: the store stops rather than keep the value another way.
    fn cannot_compress(&mut self, kind: Kind, why: String) {
        let dir = self.tiers[0].dir.clone();
        self.fail(dir, format!("compressing a {} value: {why}", kind.name()));
    }

    fn dictionary_of(&self, kind: Kind) -> Option<u32> {
        self.tiers[0]
            .dictionaries
            .iter()
            .rposition(|it| it.kind == kind && it.bytes.is_ok())
            .map(|at| at as u32)
    }

    fn sample(&mut self, kind: Kind, loc: Loc) {
        let training = &mut self.training[kind.code() as usize];
        if training.samples.len() < cold::SAMPLES_MOST && training.bytes < cold::SAMPLE_BYTES_MOST {
            training.samples.push((loc.pack, loc.offset, loc.raw));
            training.bytes += loc.raw;
        }
        if training.samples.len() >= cold::TRAIN_VALUES.max(training.after)
            && training.bytes >= cold::TRAIN_BYTES
        {
            self.train(kind);
        }
    }

    fn train(&mut self, kind: Kind) {
        let trained = {
            let own = &self.tiers[0];
            let samples: Vec<&[u8]> = self.training[kind.code() as usize]
                .samples
                .iter()
                .filter_map(|&(pack, offset, raw)| {
                    own.packs[pack as usize].bytes(offset + RECORD_HEADER, raw)
                })
                .collect();
            cold::train(&samples)
        };
        let training = &mut self.training[kind.code() as usize];
        match trained {
            Ok(dictionary) => {
                *training = Training::default();
                let header = RecordHeader {
                    kind,
                    form: Form::Dictionary,
                    dictionary: 0,
                    stored: dictionary.len() as u64,
                    raw: dictionary.len() as u64,
                    key: Key(ContentId::of(DICTIONARY_DOMAIN, &dictionary)),
                    content: content_of(&dictionary),
                };
                let loc = self.append_record(header, &dictionary);
                self.tiers[0].dictionaries.push(Dictionary {
                    kind,
                    pack: loc.pack,
                    offset: loc.offset,
                    bytes: Ok(dictionary),
                });
            }
            Err(why) => {
                training.after = training.samples.len() * 2;
                self.problems.push(format!("the {} values are kept raw: {why}", kind.name()));
            }
        }
    }

    fn start_commit(&mut self, effects: &mut Vec<Effect>) {
        debug_assert!(self.commit.is_none());
        for pack in &mut self.tiers[0].packs {
            if pack.end > pack.committed {
                let padded = disk::blocks(pack.end);
                pack.memory.resize(pack.memory.len() + (padded - pack.end) as usize, 0);
                pack.end = padded;
            }
        }
        let own = &self.tiers[0];
        let packs: Vec<(usize, u64)> = own
            .packs
            .iter()
            .enumerate()
            .filter(|(_, it)| it.end > it.committed)
            .map(|(at, it)| (at, it.end))
            .collect();
        let mut externals: Vec<ContentId> =
            own.externals.iter().filter(|(_, it)| !it.committed).map(|(it, _)| *it).collect();
        externals.sort_unstable();
        let compact = !self.index_mapping && own.delta.len() >= INDEX_AFTER;
        if packs.is_empty() && externals.is_empty() && !compact {
            return;
        }
        let mut all_externals: Vec<ExternalRef> = own
            .externals
            .iter()
            .map(|(content, it)| ExternalRef { content: *content, len: it.len })
            .collect();
        all_externals.sort_unstable_by_key(|it| it.content);
        let mut head = Head {
            generation: own.head.generation + 1,
            index: own.head.index,
            packs: own
                .packs
                .iter()
                .map(|it| PackRef {
                    kind: it.kind,
                    number: it.number,
                    committed: it.end,
                    indexed: it.indexed,
                })
                .collect(),
            dictionaries: own
                .dictionaries
                .iter()
                .map(|it| DictionaryRef { kind: it.kind, pack: it.pack, offset: it.offset })
                .collect(),
            externals: all_externals,
        };
        let mut index = None;
        if compact {
            let mut entries = Vec::with_capacity(own.delta.len());
            if let Some(table) = own.table() {
                entries.reserve(table.count() as usize);
                for (key, loc) in table.iter() {
                    match loc {
                        Ok(_) if own.delta.contains_key(&key) => {}
                        Ok(loc) => entries.push((key, loc)),
                        Err(what) => self.problems.push(format!(
                            "a slot of {} left out of the next index: {what}",
                            own.index_file()
                        )),
                    }
                }
            }
            entries.extend(own.delta.iter().map(|(key, (loc, _))| (*key, *loc)));
            entries.sort_unstable_by_key(|(key, _)| *key);
            let bytes = disk::build_index(&entries);
            let slot = match own.head.index {
                Some(it) => 1 - it.slot,
                None => 0,
            };
            head.index = Some(IndexRef {
                slot,
                len: bytes.len() as u64,
                check: crc::crc32c(&bytes),
                count: entries.len() as u64,
            });
            for pack in &mut head.packs {
                pack.indexed = pack.committed;
            }
            index = Some((slot, bytes));
        }
        let mut steps = 0;
        for &(at, _) in &packs {
            let pack = &own.packs[at];
            let tag = self.tags.ask(Op::Truncate { pack: at });
            effects.push(Effect::Truncate { file: pack.file.clone(), len: pack.committed, tag });
            steps += 1;
        }
        for content in &externals {
            let external = &own.externals[content];
            let bytes = external.memory.clone().expect("a file not yet written is held");
            let tag = self.tags.ask(Op::WriteExternal { content: *content });
            effects.push(Effect::Write { file: external.file.clone(), bytes, tag });
            steps += 1;
        }
        let index = index.map(|(slot, bytes)| {
            let tag = self.tags.ask(Op::WriteIndex);
            effects.push(Effect::Write { file: own.file(&disk::index_file(slot)), bytes, tag });
            steps += 1;
            (slot, self.seq)
        });
        self.commit = Some(Commit {
            packs,
            externals,
            index,
            head,
            tickets: self.next_ticket - 1,
            steps,
            failed: None,
            head_sent: false,
        });
    }

    fn step(&mut self, op: Op, done: Done, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        let Some(commit) = self.commit.as_mut() else { return Vec::new() };
        commit.steps -= 1;
        let own = &self.tiers[0];
        let file_of = |op: &Op| match op {
            Op::Truncate { pack } | Op::Append { pack } | Op::Sync { pack } => {
                own.packs[*pack].file.clone()
            }
            Op::WriteExternal { content } => own.externals[content].file.clone(),
            Op::WriteIndex => own.file(&disk::index_file(commit.index.map_or(0, |it| it.0))),
            _ => own.file(HEAD_FILE),
        };
        match (&op, &done) {
            (Op::Truncate { pack }, Done::Truncated) => {
                if commit.failed.is_none() {
                    let at = *pack;
                    let pack = &own.packs[at];
                    let to = commit.to(at).expect("a pack the commit truncated it writes");
                    let bytes = pack.memory
                        [(pack.committed - pack.usable) as usize..(to - pack.usable) as usize]
                        .to_vec();
                    let tag = self.tags.ask(Op::Append { pack: at });
                    effects.push(Effect::Append { file: pack.file.clone(), bytes, tag });
                    commit.steps += 1;
                }
            }
            (Op::Append { pack }, Done::Appended) => {
                if commit.failed.is_none() {
                    let tag = self.tags.ask(Op::Sync { pack: *pack });
                    effects.push(Effect::Sync { file: own.packs[*pack].file.clone(), tag });
                    commit.steps += 1;
                }
            }
            (Op::Sync { .. }, Done::Synced)
            | (Op::WriteExternal { .. } | Op::WriteIndex, Done::Written) => {}
            (Op::WriteHead, Done::Written) => return self.finish_commit(effects),
            (op, done) => {
                if commit.failed.is_none() {
                    commit.failed = Some(Broken { file: file_of(op), what: describe(done) });
                }
            }
        }
        if commit.steps > 0 {
            return Vec::new();
        }
        if let Some(failed) = commit.failed.take() {
            self.commit = None;
            self.failures += 1;
            if self.failures >= FAILURES_MOST {
                self.broken.get_or_insert(failed);
            } else {
                self.start_commit(effects);
            }
            return Vec::new();
        }
        if !commit.head_sent {
            commit.head_sent = true;
            commit.steps += 1;
            let tag = self.tags.ask(Op::WriteHead);
            effects.push(Effect::Write { file: own.file(HEAD_FILE), bytes: commit.head.encode(), tag });
        }
        Vec::new()
    }

    fn finish_commit(&mut self, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        let commit = self.commit.take().expect("a commit in flight");
        let own = &mut self.tiers[0];
        for &(at, to) in &commit.packs {
            own.packs[at].committed = to;
        }
        for (pack, written) in own.packs.iter_mut().zip(&commit.head.packs) {
            pack.indexed = written.indexed;
        }
        own.head = commit.head;
        let mut maps = Vec::new();
        for content in &commit.externals {
            let external = own.externals.get_mut(content).expect("a file the commit wrote");
            external.committed = true;
            maps.push((external.file.clone(), Mapped::External { content: *content }));
        }
        for &(at, to) in &commit.packs {
            maps.push((own.packs[at].file.clone(), Mapped::Pack { pack: at, usable: to }));
        }
        if let Some((slot, seq)) = commit.index {
            maps.push((own.file(&disk::index_file(slot)), Mapped::Index { seq: Some(seq) }));
            self.index_mapping = true;
        }
        for (file, what) in maps {
            self.ask_map(0, file, what, effects);
        }
        self.failures = 0;
        let tickets = (self.durable + 1..=commit.tickets).map(Ticket).collect();
        self.durable = self.durable.max(commit.tickets);
        if std::mem::take(&mut self.flush_wanted) {
            self.start_commit(effects);
        }
        tickets
    }

    // Opening and mapping.

    fn fail(&mut self, file: FileName, what: impl Into<String>) {
        self.broken.get_or_insert(Broken { file, what: what.into() });
    }

    /// Whether to ask again after IO failed.
    fn retry(&mut self, file: &FileName, done: &Done) -> bool {
        self.failures += 1;
        if self.failures >= FAILURES_MOST {
            self.fail(file.clone(), describe(done));
            return false;
        }
        true
    }

    fn ask_map(&mut self, tier: usize, file: FileName, what: Mapped, effects: &mut Vec<Effect>) {
        let tag = self.tags.ask(Op::Map { tier, file: file.clone(), what });
        effects.push(Effect::Map { file, tag });
    }

    fn read_head(&mut self, at: usize, bytes: Vec<u8>, effects: &mut Vec<Effect>) {
        let file = self.tiers[at].file(HEAD_FILE);
        let head = match (bytes.is_empty(), at) {
            (true, 0) => Head::default(),
            (true, _) => return self.fail(file, "it is not there: a layer is a store"),
            (false, _) => match Head::decode(&bytes) {
                Ok(head) => head,
                Err(why) => return self.fail(file, why),
            },
        };
        if head.packs.len() > PACKS_MOST {
            return self.fail(file, format!("it names more than {PACKS_MOST} packs"));
        }
        let tier = &mut self.tiers[at];
        let packs: Vec<Pack> = head
            .packs
            .iter()
            .map(|it| Pack {
                kind: it.kind,
                number: it.number,
                file: tier.file(&disk::pack_file(it.kind, it.number)),
                mapping: None,
                usable: it.committed,
                memory: Vec::new(),
                committed: it.committed,
                indexed: it.indexed,
                end: it.committed,
            })
            .collect();
        let externals: FxHashMap<ContentId, External> = head
            .externals
            .iter()
            .map(|it| {
                (it.content, External {
                    len: it.len,
                    file: tier.file(&disk::external_file(&it.content)),
                    mapping: None,
                    memory: None,
                    committed: true,
                })
            })
            .collect();
        let mut maps = Vec::new();
        if let Some(index) = head.index {
            maps.push((tier.file(&disk::index_file(index.slot)), Mapped::Index { seq: None }));
        }
        for (pack, it) in packs.iter().enumerate() {
            maps.push((it.file.clone(), Mapped::Pack { pack, usable: it.committed }));
        }
        let mut contents: Vec<&ContentId> = externals.keys().collect();
        contents.sort_unstable();
        for content in contents {
            maps.push((externals[content].file.clone(), Mapped::External { content: *content }));
        }
        tier.dictionaries = head
            .dictionaries
            .iter()
            .map(|it| Dictionary {
                kind: it.kind,
                pack: it.pack,
                offset: it.offset,
                bytes: Err("was not read".into()),
            })
            .collect();
        tier.packs = packs;
        tier.externals = externals;
        tier.head = head;
        tier.head_read = true;
        tier.waiting = maps.len();
        for (file, what) in maps {
            self.ask_map(at, file, what, effects);
        }
        if self.tiers[at].waiting == 0 {
            self.finish_open(at);
        }
    }

    fn install(&mut self, at: usize, file: FileName, what: Mapped, mapping: Mapping) {
        let opening = !self.tiers[at].opened;
        let len = mapping.bytes().len() as u64;
        let tier = &mut self.tiers[at];
        match what {
            Mapped::Pack { pack, usable } => {
                if len < usable {
                    return self.fail(
                        file,
                        format!("it is {len} bytes, shorter than the {usable} its head names"),
                    );
                }
                let pack = &mut tier.packs[pack];
                if usable >= pack.usable {
                    pack.memory.drain(..(usable - pack.usable) as usize);
                    pack.usable = usable;
                    pack.mapping = Some(mapping);
                }
            }
            Mapped::External { content } => {
                let Some(external) = tier.externals.get_mut(&content) else { return };
                if len != external.len {
                    return self.fail(
                        file,
                        format!("it is {len} bytes, not the {} its head names", external.len),
                    );
                }
                external.mapping = Some(mapping);
                external.memory = None;
            }
            Mapped::Index { seq } => {
                if let Err(why) = Table::new(mapping.bytes()) {
                    return self.fail(file, why);
                }
                match seq {
                    None => tier.index = Some(mapping),
                    Some(seq) => {
                        tier.index = Some(mapping);
                        tier.delta.retain(|_, (_, put)| *put > seq);
                        self.index_mapping = false;
                    }
                }
            }
        }
        self.failures = 0;
        if opening {
            let tier = &mut self.tiers[at];
            tier.waiting -= 1;
            if tier.waiting == 0 && tier.head_read {
                self.finish_open(at);
            }
        }
    }

    fn finish_open(&mut self, at: usize) {
        match open_tier(&mut self.tiers[at]) {
            Ok(()) => self.tiers[at].opened = true,
            Err((file, why)) => self.fail(file, why),
        }
    }

    fn take_fetched(&mut self, key: Key, done: Done) -> Fetched {
        let bytes = match done {
            Done::Read(bytes) => bytes,
            done => return Fetched::Refused(describe(&done)),
        };
        match remote::Answer::decode(&bytes) {
            Err(why) => Fetched::Refused(why),
            Ok(remote::Answer::Missing { key: it }) if it == key => Fetched::Missing,
            Ok(remote::Answer::Value { key: it, kind, content, bytes }) if it == key => {
                let found = content_of(&bytes);
                if found != content {
                    return Fetched::Refused(format!(
                        "its bytes hash to {found:x}, not the {content:x} they were sent as"
                    ));
                }
                if !self.tiers[0].opened || self.broken.is_some() {
                    return Fetched::Refused("the store is not open".into());
                }
                match self.put(key, kind, bytes) {
                    Ok(ticket) => Fetched::Kept(ticket),
                    Err(mismatch) => Fetched::Refused(format!(
                        "the store keeps {:x} under it, not the {:x} sent",
                        mismatch.kept, mismatch.given
                    )),
                }
            }
            Ok(_) => Fetched::Refused("the answer is for another key".into()),
        }
    }
}

/// Checks what a tier's head names against what was mapped, and reads the records past its
/// index table and its dictionaries.
fn open_tier(tier: &mut Tier) -> Result<(), (FileName, String)> {
    if let Some(index) = tier.head.index {
        let file = tier.index_file();
        let mapping =
            tier.index.as_ref().ok_or_else(|| (file.clone(), "it was not mapped".to_owned()))?;
        if mapping.bytes().len() as u64 != index.len {
            return Err((file, format!("it is not the {} bytes its head names", index.len)));
        }
        let check = crc::crc32c(mapping.bytes());
        if check != index.check {
            return Err((file, format!("its CRC-32C is {check:08x}, not the {:08x} its head names", index.check)));
        }
        let table = Table::new(mapping.bytes()).map_err(|why| (file.clone(), why.to_owned()))?;
        if table.count() != index.count {
            return Err((file, format!("it holds {} values, not the {} its head names", table.count(), index.count)));
        }
    }
    let mut delta = FxHashMap::default();
    for (number, pack) in tier.packs.iter().enumerate() {
        let fail = |why: String| (pack.file.clone(), why);
        let all = pack.mapping.as_ref().ok_or_else(|| fail("it was not mapped".into()))?.bytes();
        let all = &all[..pack.committed as usize];
        disk::check_pack_header(all, pack.kind).map_err(|why| fail(why.into()))?;
        let mut at = pack.indexed.max(PACK_HEADER);
        while at < pack.committed {
            if all[at as usize..at as usize + 4] == [0; 4] {
                let next = disk::blocks(at);
                if next == at || all[at as usize..next as usize].iter().any(|&it| it != 0) {
                    return Err(fail(format!("at {at} is neither a record nor the zeros that end a commit")));
                }
                at = next;
                continue;
            }
            let header = RecordHeader::decode(&all[at as usize..])
                .map_err(|why| fail(format!("the record at {at}: {why}")))?;
            let end = at.checked_add(header.span()).filter(|&end| end <= pack.committed).ok_or_else(
                || fail(format!("the record at {at} runs past the {} bytes its head names", pack.committed)),
            )?;
            if header.kind != pack.kind {
                return Err(fail(format!("the record at {at} is of another kind than its pack")));
            }
            if header.form != Form::Dictionary {
                let loc = Loc {
                    pack: number as u32,
                    kind: header.kind,
                    form: header.form,
                    offset: at,
                    stored: header.stored,
                    raw: header.raw,
                };
                delta.insert(header.key, (loc, 0));
            }
            at = end;
        }
    }
    tier.delta = delta;
    let packs = &tier.packs;
    for dictionary in &mut tier.dictionaries {
        let pack = &packs[dictionary.pack as usize];
        let at = dictionary.offset;
        dictionary.bytes = pack
            .bytes(at, RECORD_HEADER)
            .ok_or_else(|| format!("at {at} is outside {}", pack.file))
            .and_then(|bytes| RecordHeader::decode(bytes).map_err(|why| format!("at {at} of {}: {why}", pack.file)))
            .and_then(|header| match header.form == Form::Dictionary && header.kind == dictionary.kind {
                true => pack
                    .bytes(at + RECORD_HEADER, header.stored)
                    .filter(|bytes| content_of(bytes) == header.content)
                    .map(<[u8]>::to_vec)
                    .ok_or_else(|| format!("at {at} of {} is not what was recorded", pack.file)),
                false => Err(format!("at {at} of {} is not a dictionary of its kind", pack.file)),
            });
    }
    Ok(())
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
