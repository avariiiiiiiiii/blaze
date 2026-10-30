//! The image's symbol table and the interners under it: names, 32-byte ids and symbol keys,
//! each interned once to a `u32`.
//!
//! Each table is 64 shards of a SwissTable (`hashbrown::HashTable`: 16 control bytes probed
//! at once with SSE2 or NEON) holding `u32` indexes, with each entry's hash kept beside it so
//! growing never hashes again. A key is hashed once; bits 32..38 of that hash pick its shard
//! and the rest place it in the shard. A batch ([`Symbols::intern_all`],
//! [`Symbols::intern_wide`]) is partitioned by shard and each shard interned by one task, so
//! nothing is locked and nothing is hashed twice; within a shard keys go in the batch's order,
//! so the ids are the same whatever the core count. A lookup is one hash, one probe of a
//! group and one compare of a 20-byte key (or the name's bytes).

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::hash::Hasher as _;

use blaze_codegen::InstanceKey;
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use rayon::prelude::*;
use rustc_hash::FxHasher;

use crate::model::Sym;
use crate::reloc::GotKind;

/// Fixed, not chosen from the core count: symbol ids order resolution's sets, and the same
/// inputs must give the same ids on every machine.
pub(crate) const SHARDS: usize = 64;
const SHIFT: u32 = 26;
const LOCAL: u32 = (1 << SHIFT) - 1;
/// Below this many keys a batch is interned on the calling thread (in the same order, so to
/// the same ids).
const PARALLEL: usize = 4096;

/// No symbol: an empty cell of the jump table.
pub(crate) const NO_SYM: Sym = Sym(u32::MAX);

/// An interned name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct NameId(pub u32);

/// An interned [`ContentId`] (also an [`InstanceKey`]'s or an [`ItemKey`]'s).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct IdNo(pub u32);

/// A native object: its input's id, and its member when the input is an archive (0
/// otherwise). Ordered by interning, not by content.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct Obj {
    pub native: IdNo,
    pub member: u32,
}

/// What a symbol is, its payload interned: 20 bytes, compared and hashed as five words.
/// Keys have no `Ord` (their ids are in interning order): [`Symbols::cmp`] orders them by
/// content, as placement does.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum SymKey {
    /// A function's code, by its instance key.
    Body(IdNo),
    /// A function's jump-table stub: what its callers call.
    Slot(IdNo),
    /// Data a function's code refers to, by its contents.
    Data(IdNo),
    /// A static: its item and its instance.
    Static(IdNo, IdNo),
    /// The Mach-O TLV descriptor of a thread-local static.
    Tlv(IdNo, IdNo),
    /// A name the image's pieces and native objects share: an extern, an export, a native
    /// global.
    Name(NameId),
    /// A native object's local symbol, by its index.
    Local(Obj, u32),
    /// A native object's section, by its index.
    Section(Obj, u32),
    /// The `core::panic::Location` of a span: its anchor and index.
    Position(IdNo, u32),
    /// A source file's name, NUL-terminated, for the locations of its spans.
    FileName(NameId),
    /// A function's CIE (by its bytes and personality) and FDE.
    Cie(IdNo),
    Fde(IdNo),
    /// A function's Windows `UNWIND_INFO` and its language-specific data.
    WinInfo(IdNo),
    /// A native object's call frame record: its section, and the byte it starts at in it.
    Frame(Obj, u32, u32),
    /// A GOT cell of a symbol, of a kind.
    Got(GotKind, Sym),
    /// A PLT stub of an import.
    Plt(Sym),
    /// The zero cell after a DLL's cells in an import address table, by the DLL's name.
    IatEnd(NameId),
    /// A tentative definition merged by name.
    Common(NameId),
    /// The cell holding the address of the current jump table.
    Generation,
    /// The jump table's cells.
    Cells,
    /// The jump table's stubs.
    Stubs,
    /// The live descriptor, and the path of the directory it names.
    Descriptor,
    LiveDir,
}

const _: () = assert!(size_of::<SymKey>() == 20);
const _: () = assert!(size_of::<Obj>() == 8);

impl SymKey {
    /// The variant, then up to four payload words: what the key is hashed as.
    #[inline]
    fn words(self) -> [u32; 5] {
        use SymKey::*;
        match self {
            Body(a) => [0, a.0, 0, 0, 0],
            Slot(a) => [1, a.0, 0, 0, 0],
            Data(a) => [2, a.0, 0, 0, 0],
            Static(a, b) => [3, a.0, b.0, 0, 0],
            Tlv(a, b) => [4, a.0, b.0, 0, 0],
            Name(a) => [5, a.0, 0, 0, 0],
            Local(o, a) => [6, o.native.0, o.member, a, 0],
            Section(o, a) => [7, o.native.0, o.member, a, 0],
            Position(a, b) => [8, a.0, b, 0, 0],
            FileName(a) => [9, a.0, 0, 0, 0],
            Cie(a) => [10, a.0, 0, 0, 0],
            Fde(a) => [11, a.0, 0, 0, 0],
            WinInfo(a) => [12, a.0, 0, 0, 0],
            Frame(o, a, b) => [13, o.native.0, o.member, a, b],
            Got(kind, a) => [14, kind as u32, a.0, 0, 0],
            Plt(a) => [15, a.0, 0, 0, 0],
            IatEnd(a) => [16, a.0, 0, 0, 0],
            Common(a) => [17, a.0, 0, 0, 0],
            Generation => [18, 0, 0, 0, 0],
            Cells => [19, 0, 0, 0, 0],
            Stubs => [20, 0, 0, 0, 0],
            Descriptor => [21, 0, 0, 0, 0],
            LiveDir => [22, 0, 0, 0, 0],
        }
    }
}

/// A key as parallel code builds it without the table: its ids and name not yet interned,
/// borrowed from codegen's data. [`Symbols::intern_wide`] interns a batch of them.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Wide<'a> {
    Key(SymKey),
    Name(&'a str),
    Body(&'a InstanceKey),
    Slot(&'a InstanceKey),
    Data(&'a ContentId),
    Static(&'a ItemKey, &'a ContentId),
    Position(&'a SpanRef),
}

const K0: u64 = 0x9e37_79b9_7f4a_7c15;
const K1: u64 = 0xc2b2_ae3d_27d4_eb4f;
const K2: u64 = 0x1656_67b1_9e37_79f9;

/// The high and low halves of the 128-bit product, folded (wyhash's and foldhash's mixer).
#[inline]
fn fold(a: u64, b: u64) -> u64 {
    let product = u128::from(a) * u128::from(b);
    (product as u64) ^ ((product >> 64) as u64)
}

#[inline]
fn hash_key(key: SymKey) -> u64 {
    let w = key.words();
    let a = u64::from(w[0]) | (u64::from(w[1]) << 32);
    let b = u64::from(w[2]) | (u64::from(w[3]) << 32);
    fold(fold(a ^ K0, b ^ K1) ^ u64::from(w[4]), K2)
}

#[inline]
fn hash_name(name: &str) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write(name.as_bytes());
    fold(hasher.finish() ^ name.len() as u64, K0)
}

/// A content id is a BLAKE3 hash: its first eight bytes are its hash.
#[inline]
fn hash_id(id: &ContentId) -> u64 {
    u64::from_le_bytes(id.0[..8].try_into().expect("eight bytes"))
}

#[inline]
fn shard_of(hash: u64) -> usize {
    (hash >> 32) as usize & (SHARDS - 1)
}

#[inline]
fn join(shard: usize, local: u32) -> u32 {
    assert!(local <= LOCAL, "more than 2^26 entries in one shard of a symbol table");
    ((shard as u32) << SHIFT) | local
}

#[inline]
fn split(it: u32) -> (usize, usize) {
    ((it >> SHIFT) as usize, (it & LOCAL) as usize)
}

/// One shard of one table: finds a key or adds it, returning its index in the shard.
trait Shard<K>: Send {
    fn get_or_insert(&mut self, hash: u64, key: K) -> u32;
}

#[derive(Clone, Default)]
struct KeyShard {
    table: HashTable<u32>,
    hashes: Vec<u64>,
    keys: Vec<SymKey>,
}

impl KeyShard {
    #[inline]
    fn find(&self, hash: u64, key: SymKey) -> Option<u32> {
        self.table.find(hash, |&at| self.keys[at as usize] == key).copied()
    }
}

impl Shard<SymKey> for KeyShard {
    #[inline]
    fn get_or_insert(&mut self, hash: u64, key: SymKey) -> u32 {
        let (keys, hashes) = (&self.keys, &self.hashes);
        match self.table.entry(hash, |&at| keys[at as usize] == key, |&at| hashes[at as usize]) {
            Entry::Occupied(it) => *it.get(),
            Entry::Vacant(it) => {
                let at = self.keys.len() as u32;
                it.insert(at);
                self.keys.push(key);
                self.hashes.push(hash);
                at
            }
        }
    }
}

/// Names: every name of the shard in one buffer, each the bytes up to its end offset.
#[derive(Clone, Default)]
struct NameShard {
    table: HashTable<u32>,
    hashes: Vec<u64>,
    bytes: String,
    ends: Vec<u32>,
}

#[inline]
fn name_at<'s>(bytes: &'s str, ends: &[u32], at: u32) -> &'s str {
    let at = at as usize;
    let start = if at == 0 { 0 } else { ends[at - 1] as usize };
    &bytes[start..ends[at] as usize]
}

impl NameShard {
    #[inline]
    fn get(&self, at: usize) -> &str {
        name_at(&self.bytes, &self.ends, at as u32)
    }

    #[inline]
    fn find(&self, hash: u64, name: &str) -> Option<u32> {
        self.table.find(hash, |&at| name_at(&self.bytes, &self.ends, at) == name).copied()
    }
}

impl<'k> Shard<&'k str> for NameShard {
    #[inline]
    fn get_or_insert(&mut self, hash: u64, name: &'k str) -> u32 {
        let (bytes, ends, hashes) = (&self.bytes, &self.ends, &self.hashes);
        match self.table.entry(hash, |&at| name_at(bytes, ends, at) == name, |&at| hashes[at as usize]) {
            Entry::Occupied(it) => *it.get(),
            Entry::Vacant(it) => {
                let at = self.ends.len() as u32;
                it.insert(at);
                self.bytes.push_str(name);
                self.ends.push(u32::try_from(self.bytes.len()).expect("a shard of names past 4 GiB"));
                self.hashes.push(hash);
                at
            }
        }
    }
}

#[derive(Clone, Default)]
struct IdShard {
    table: HashTable<u32>,
    ids: Vec<ContentId>,
}

impl IdShard {
    #[inline]
    fn find(&self, hash: u64, id: &ContentId) -> Option<u32> {
        self.table.find(hash, |&at| self.ids[at as usize] == *id).copied()
    }
}

impl<'k> Shard<&'k ContentId> for IdShard {
    #[inline]
    fn get_or_insert(&mut self, hash: u64, id: &'k ContentId) -> u32 {
        let ids = &self.ids;
        match self.table.entry(hash, |&at| ids[at as usize] == *id, |&at| hash_id(&ids[at as usize])) {
            Entry::Occupied(it) => *it.get(),
            Entry::Vacant(it) => {
                let at = self.ids.len() as u32;
                it.insert(at);
                self.ids.push(*id);
                at
            }
        }
    }
}

/// Interns a batch: each key hashed once, partitioned by shard, each shard's keys interned in
/// the batch's order by one task. Returns the joined (shard, index) of each key.
fn batch<K, S>(shards: &mut [S], keys: &[K], hash: impl Fn(K) -> u64 + Sync) -> Vec<u32>
where
    K: Copy + Sync,
    S: Shard<K>,
{
    if keys.len() < PARALLEL {
        return keys
            .iter()
            .map(|key| {
                let hash = hash(*key);
                let shard = shard_of(hash);
                join(shard, shards[shard].get_or_insert(hash, *key))
            })
            .collect();
    }
    let hashes: Vec<u64> = keys.par_iter().map(|key| hash(*key)).collect();
    let mut by_shard: Vec<Vec<(u32, u64)>> = vec![Vec::new(); SHARDS];
    for (at, hash) in hashes.into_iter().enumerate() {
        by_shard[shard_of(hash)].push((at as u32, hash));
    }
    shards.par_iter_mut().zip(by_shard.par_iter_mut()).enumerate().for_each(|(shard, (it, items))| {
        for item in items.iter_mut() {
            item.1 = u64::from(join(shard, it.get_or_insert(item.1, keys[item.0 as usize])));
        }
    });
    let mut out = vec![0; keys.len()];
    for (at, it) in by_shard.into_iter().flatten() {
        out[at as usize] = it as u32;
    }
    out
}

/// The image's symbols, names and ids. Append-only for the image's life: a failed stage's
/// symbols stay, unused.
#[derive(Clone)]
pub(crate) struct Symbols {
    keys: Vec<KeyShard>,
    names: Vec<NameShard>,
    ids: Vec<IdShard>,
}

impl Default for Symbols {
    fn default() -> Self {
        Symbols {
            keys: vec![KeyShard::default(); SHARDS],
            names: vec![NameShard::default(); SHARDS],
            ids: vec![IdShard::default(); SHARDS],
        }
    }
}

impl Symbols {
    pub const SHARDS: usize = SHARDS;

    pub fn name(&mut self, name: &str) -> NameId {
        let hash = hash_name(name);
        let shard = shard_of(hash);
        NameId(join(shard, self.names[shard].get_or_insert(hash, name)))
    }

    pub fn find_name(&self, name: &str) -> Option<NameId> {
        let hash = hash_name(name);
        let shard = shard_of(hash);
        self.names[shard].find(hash, name).map(|at| NameId(join(shard, at)))
    }

    pub fn str(&self, name: NameId) -> &str {
        let (shard, at) = split(name.0);
        self.names[shard].get(at)
    }

    pub fn names_all(&mut self, names: &[&str]) -> Vec<NameId> {
        batch(&mut self.names, names, hash_name).into_iter().map(NameId).collect()
    }

    pub fn id(&mut self, id: &ContentId) -> IdNo {
        let hash = hash_id(id);
        let shard = shard_of(hash);
        IdNo(join(shard, self.ids[shard].get_or_insert(hash, id)))
    }

    pub fn find_id(&self, id: &ContentId) -> Option<IdNo> {
        let hash = hash_id(id);
        let shard = shard_of(hash);
        self.ids[shard].find(hash, id).map(|at| IdNo(join(shard, at)))
    }

    pub fn ids_all(&mut self, ids: &[&ContentId]) -> Vec<IdNo> {
        batch(&mut self.ids, ids, hash_id).into_iter().map(IdNo).collect()
    }

    pub fn content(&self, id: IdNo) -> &ContentId {
        let (shard, at) = split(id.0);
        &self.ids[shard].ids[at]
    }

    pub fn instance(&self, id: IdNo) -> InstanceKey {
        InstanceKey(*self.content(id))
    }

    pub fn item(&self, id: IdNo) -> ItemKey {
        ItemKey(*self.content(id))
    }

    pub fn span(&self, anchor: IdNo, index: u32) -> SpanRef {
        SpanRef { anchor: *self.content(anchor), index }
    }

    pub fn obj(&mut self, native: &ContentId, member: u32) -> Obj {
        Obj { native: self.id(native), member }
    }

    pub fn intern(&mut self, key: SymKey) -> Sym {
        let hash = hash_key(key);
        let shard = shard_of(hash);
        Sym(join(shard, self.keys[shard].get_or_insert(hash, key)))
    }

    /// Interns every key, the shards in parallel; the symbols in the keys' order.
    pub fn intern_all(&mut self, keys: &[SymKey]) -> Vec<Sym> {
        batch(&mut self.keys, keys, hash_key).into_iter().map(Sym).collect()
    }

    /// Interns keys whose ids and names are not interned yet: the ids, then the names, then
    /// the keys, each a batch by shard.
    pub fn intern_wide(&mut self, keys: &[Wide<'_>]) -> Vec<Sym> {
        let mut ids: Vec<&ContentId> = Vec::with_capacity(keys.len());
        let mut names: Vec<&str> = Vec::new();
        for key in keys {
            match *key {
                Wide::Key(_) => {}
                Wide::Name(name) => names.push(name),
                Wide::Body(it) | Wide::Slot(it) => ids.push(&it.0),
                Wide::Data(it) => ids.push(it),
                Wide::Static(item, instance) => {
                    ids.push(&item.0);
                    ids.push(instance);
                }
                Wide::Position(span) => ids.push(&span.anchor),
            }
        }
        let ids = self.ids_all(&ids);
        let names = self.names_all(&names);
        let (mut id, mut name) = (ids.iter().copied(), names.iter().copied());
        let mut next = || id.next().expect("one id per id of the keys");
        let compact: Vec<SymKey> = keys
            .iter()
            .map(|key| match *key {
                Wide::Key(key) => key,
                Wide::Name(_) => SymKey::Name(name.next().expect("one name per name of the keys")),
                Wide::Body(_) => SymKey::Body(next()),
                Wide::Slot(_) => SymKey::Slot(next()),
                Wide::Data(_) => SymKey::Data(next()),
                Wide::Static(..) => {
                    let item = next();
                    SymKey::Static(item, next())
                }
                Wide::Position(span) => SymKey::Position(next(), span.index),
            })
            .collect();
        self.intern_all(&compact)
    }

    pub fn find(&self, key: SymKey) -> Option<Sym> {
        let hash = hash_key(key);
        let shard = shard_of(hash);
        self.keys[shard].find(hash, key).map(|at| Sym(join(shard, at)))
    }

    #[inline]
    pub fn key(&self, sym: Sym) -> SymKey {
        let (shard, at) = split(sym.0);
        self.keys[shard].keys[at]
    }

    pub fn sym_name(&mut self, name: &str) -> Sym {
        let name = self.name(name);
        self.intern(SymKey::Name(name))
    }

    pub fn find_sym_name(&self, name: &str) -> Option<Sym> {
        self.find(SymKey::Name(self.find_name(name)?))
    }

    /// The name a symbol is, when its key is a name.
    pub fn name_of(&self, sym: Sym) -> Option<&str> {
        match self.key(sym) {
            SymKey::Name(name) => Some(self.str(name)),
            _ => None,
        }
    }

    pub fn body(&mut self, key: &InstanceKey) -> Sym {
        let id = self.id(&key.0);
        self.intern(SymKey::Body(id))
    }

    pub fn find_body(&self, key: &InstanceKey) -> Option<Sym> {
        self.find(SymKey::Body(self.find_id(&key.0)?))
    }

    pub fn find_slot(&self, key: &InstanceKey) -> Option<Sym> {
        self.find(SymKey::Slot(self.find_id(&key.0)?))
    }

    /// Two symbols in the order of their contents: the variant, then names as strings and ids
    /// as bytes (what the keys' derived order was when they held them).
    pub fn cmp(&self, a: Sym, b: Sym) -> Ordering {
        if a == b {
            return Ordering::Equal;
        }
        self.cmp_keys(self.key(a), self.key(b))
    }

    pub fn cmp_keys(&self, a: SymKey, b: SymKey) -> Ordering {
        use SymKey::*;
        let id = |x: IdNo, y: IdNo| if x == y { Ordering::Equal } else { self.content(x).cmp(self.content(y)) };
        let name = |x: NameId, y: NameId| if x == y { Ordering::Equal } else { self.str(x).cmp(self.str(y)) };
        let obj = |x: Obj, y: Obj| id(x.native, y.native).then(x.member.cmp(&y.member));
        match (a, b) {
            (Body(x), Body(y))
            | (Slot(x), Slot(y))
            | (Data(x), Data(y))
            | (Cie(x), Cie(y))
            | (Fde(x), Fde(y))
            | (WinInfo(x), WinInfo(y)) => id(x, y),
            (Static(x, i), Static(y, j)) | (Tlv(x, i), Tlv(y, j)) => id(x, y).then_with(|| id(i, j)),
            (Name(x), Name(y)) | (FileName(x), FileName(y)) | (IatEnd(x), IatEnd(y)) | (Common(x), Common(y)) => {
                name(x, y)
            }
            (Local(x, i), Local(y, j)) | (Section(x, i), Section(y, j)) => obj(x, y).then(i.cmp(&j)),
            (Position(x, i), Position(y, j)) => id(x, y).then(i.cmp(&j)),
            (Frame(x, s, i), Frame(y, t, j)) => obj(x, y).then(s.cmp(&t)).then(i.cmp(&j)),
            (Got(k, x), Got(l, y)) => k.cmp(&l).then_with(|| self.cmp(x, y)),
            (Plt(x), Plt(y)) => self.cmp(x, y),
            _ => a.words()[0].cmp(&b.words()[0]),
        }
    }

    /// A symbol as an error names it.
    pub fn describe(&self, sym: Sym) -> String {
        match self.key(sym) {
            SymKey::Body(key) | SymKey::Slot(key) => format!("the function {:?}", self.instance(key)),
            SymKey::Static(item, instance) => {
                format!("the static {:?} ({:?})", self.item(item), self.content(instance))
            }
            SymKey::Name(name) => format!("`{}`", self.str(name)),
            SymKey::Data(id) => format!("the data {:?}", self.content(id)),
            key => self.debug(key),
        }
    }

    /// A key written out with its contents, as `{:?}` of the key once was.
    pub fn debug(&self, key: SymKey) -> String {
        let obj = |o: Obj| format!("Obj {{ native: {:?}, member: {} }}", self.content(o.native), o.member);
        let span = |anchor: IdNo, index: u32| format!("{:?}", self.span(anchor, index));
        let mut out = String::new();
        let _ = match key {
            SymKey::Body(it) => write!(out, "Body({:?})", self.instance(it)),
            SymKey::Slot(it) => write!(out, "Slot({:?})", self.instance(it)),
            SymKey::Data(it) => write!(out, "Data({:?})", self.content(it)),
            SymKey::Static(a, b) => write!(out, "Static({:?}, {:?})", self.item(a), self.content(b)),
            SymKey::Tlv(a, b) => write!(out, "Tlv({:?}, {:?})", self.item(a), self.content(b)),
            SymKey::Name(it) => write!(out, "Name({:?})", self.str(it)),
            SymKey::Local(o, at) => write!(out, "Local({}, {at})", obj(o)),
            SymKey::Section(o, at) => write!(out, "Section({}, {at})", obj(o)),
            SymKey::Position(anchor, index) => write!(out, "Position({})", span(anchor, index)),
            SymKey::FileName(it) => write!(out, "FileName({:?})", self.str(it)),
            SymKey::Cie(it) => write!(out, "Cie({:?})", self.content(it)),
            SymKey::Fde(it) => write!(out, "Fde({:?})", self.instance(it)),
            SymKey::WinInfo(it) => write!(out, "WinInfo({:?})", self.instance(it)),
            SymKey::Frame(o, section, at) => write!(out, "Frame({}, {section}, {at})", obj(o)),
            SymKey::Got(kind, it) => write!(out, "Got({kind:?}, {})", self.debug(self.key(it))),
            SymKey::Plt(it) => write!(out, "Plt({})", self.debug(self.key(it))),
            SymKey::IatEnd(it) => write!(out, "IatEnd({:?})", self.str(it)),
            SymKey::Common(it) => write!(out, "Common({:?})", self.str(it)),
            SymKey::Generation => write!(out, "Generation"),
            SymKey::Cells => write!(out, "Cells"),
            SymKey::Stubs => write!(out, "Stubs"),
            SymKey::Descriptor => write!(out, "Descriptor"),
            SymKey::LiveDir => write!(out, "LiveDir"),
        };
        out
    }

    /// The symbol's key as bytes of its contents (the variant, ids' bytes, names' lengths and
    /// bytes, words little-endian): the same for the same key in any image, for hashes.
    pub fn write_key(&self, sym: Sym, out: &mut Vec<u8>) {
        let key = self.key(sym);
        let words = key.words();
        out.push(words[0] as u8);
        let id = |out: &mut Vec<u8>, it: IdNo| out.extend_from_slice(&self.content(it).0);
        let name = |out: &mut Vec<u8>, it: NameId| {
            let it = self.str(it);
            out.extend_from_slice(&(it.len() as u64).to_le_bytes());
            out.extend_from_slice(it.as_bytes());
        };
        let word = |out: &mut Vec<u8>, it: u32| out.extend_from_slice(&it.to_le_bytes());
        match key {
            SymKey::Body(a) | SymKey::Slot(a) | SymKey::Data(a) | SymKey::Cie(a) | SymKey::Fde(a) | SymKey::WinInfo(a) => {
                id(out, a)
            }
            SymKey::Static(a, b) | SymKey::Tlv(a, b) => {
                id(out, a);
                id(out, b);
            }
            SymKey::Name(a) | SymKey::FileName(a) | SymKey::IatEnd(a) | SymKey::Common(a) => name(out, a),
            SymKey::Local(o, a) | SymKey::Section(o, a) => {
                id(out, o.native);
                word(out, o.member);
                word(out, a);
            }
            SymKey::Position(a, index) => {
                id(out, a);
                word(out, index);
            }
            SymKey::Frame(o, section, at) => {
                id(out, o.native);
                word(out, o.member);
                word(out, section);
                word(out, at);
            }
            SymKey::Got(kind, target) => {
                out.push(kind as u8);
                self.write_key(target, out);
            }
            SymKey::Plt(target) => self.write_key(target, out),
            SymKey::Generation | SymKey::Cells | SymKey::Stubs | SymKey::Descriptor | SymKey::LiveDir => {}
        }
    }

    /// Where a symbol's per-symbol state is kept: its shard and its index in it.
    #[inline]
    pub fn split(sym: Sym) -> (usize, usize) {
        split(sym.0)
    }

    #[inline]
    pub fn join(shard: usize, at: usize) -> Sym {
        Sym(join(shard, at as u32))
    }

    /// Of an interned id, the same.
    #[inline]
    pub fn split_id(id: IdNo) -> (usize, usize) {
        split(id.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(n: u32) -> ContentId {
        ContentId::of("blaze v2 link symbols test", &n.to_le_bytes())
    }

    #[test]
    fn interning_is_idempotent_and_parallel_interning_agrees() {
        let mut symbols = Symbols::default();
        let a = symbols.sym_name("a");
        assert_eq!(symbols.sym_name("a"), a);
        let names: Vec<String> = (0..20_000).map(|n| format!("n{n}")).collect();
        let keys: Vec<SymKey> = names.iter().map(|it| SymKey::Name(symbols.name(it))).collect();
        let syms = symbols.intern_all(&keys);
        let again = symbols.intern_all(&keys);
        assert_eq!(syms, again);
        for (key, sym) in keys.iter().zip(&syms) {
            assert_eq!(symbols.key(*sym), *key);
            assert_eq!(symbols.find(*key), Some(*sym));
        }
        assert_eq!(symbols.name_of(a), Some("a"));
        assert_eq!(symbols.find_sym_name("none"), None);
        assert_eq!(symbols.find_sym_name("n77"), Some(syms[77]));
    }

    /// A batch above and below the parallel threshold gives the same ids: within a shard,
    /// keys go in the batch's order either way.
    #[test]
    fn ids_do_not_depend_on_how_a_batch_is_run() {
        let ids: Vec<ContentId> = (0..(2 * PARALLEL as u32)).map(content).collect();
        let wide: Vec<Wide<'_>> = ids.iter().map(Wide::Data).collect();
        let mut parallel = Symbols::default();
        let a = parallel.intern_wide(&wide);
        let mut serial = Symbols::default();
        let b: Vec<Sym> = wide.chunks(PARALLEL / 4).flat_map(|it| serial.intern_wide(it)).collect();
        let mut one = Symbols::default();
        let c: Vec<Sym> = ids.iter().map(|it| {
            let id = one.id(it);
            one.intern(SymKey::Data(id))
        }).collect();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn wide_keys_intern_their_ids_and_names() {
        let mut symbols = Symbols::default();
        let (f, item, data) = (InstanceKey(content(1)), ItemKey(content(2)), content(3));
        let span = SpanRef { anchor: content(4), index: 9 };
        let wide =
            [Wide::Body(&f), Wide::Slot(&f), Wide::Static(&item, &data), Wide::Name("memcpy"), Wide::Position(&span), Wide::Key(SymKey::Cells)];
        let syms = symbols.intern_wide(&wide);
        assert_eq!(syms[0], symbols.body(&f));
        assert_eq!(Some(syms[1]), symbols.find_slot(&f));
        let SymKey::Static(a, b) = symbols.key(syms[2]) else { panic!("a static") };
        assert_eq!((symbols.item(a), *symbols.content(b)), (item, data));
        assert_eq!(symbols.name_of(syms[3]), Some("memcpy"));
        let SymKey::Position(anchor, index) = symbols.key(syms[4]) else { panic!("a position") };
        assert_eq!(symbols.span(anchor, index), span);
        assert_eq!(symbols.key(syms[5]), SymKey::Cells);
        assert_eq!(symbols.intern_wide(&wide), syms);
    }

    /// Content order is the variant, then contents, whatever order the keys were interned in.
    #[test]
    fn keys_order_by_content() {
        let mut symbols = Symbols::default();
        let b = symbols.sym_name("b");
        let a = symbols.sym_name("a");
        let x = symbols.intern(SymKey::Generation);
        let (late, early) = (content(10), content(11));
        let (late, early) = if late < early { (early, late) } else { (late, early) };
        let first = symbols.id(&late);
        let second = symbols.id(&early);
        let d1 = symbols.intern(SymKey::Data(first));
        let d2 = symbols.intern(SymKey::Data(second));
        assert_eq!(symbols.cmp(a, b), Ordering::Less);
        assert_eq!(symbols.cmp(d2, d1), Ordering::Less);
        assert_eq!(symbols.cmp(d1, a), Ordering::Less);
        assert_eq!(symbols.cmp(a, x), Ordering::Less);
        let ga = symbols.intern(SymKey::Got(GotKind::Address, a));
        let gb = symbols.intern(SymKey::Got(GotKind::Address, b));
        assert_eq!(symbols.cmp(gb, ga), Ordering::Greater);
        let mut bytes = Vec::new();
        symbols.write_key(ga, &mut bytes);
        assert_eq!(bytes, [&[14u8, 0, 5][..], &1u64.to_le_bytes(), b"a"].concat());
        assert_eq!(symbols.describe(a), "`a`");
        assert_eq!(symbols.describe(ga), "Got(Address, Name(\"a\"))");
    }
}
