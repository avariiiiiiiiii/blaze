//! What readers read, with no lock (docs/v2/plan/design/concurrency.md "Reads").
//!
//! - The store publishes a [`View`] (its tiers' packs, index tables, delta tables, files of
//!   their own, dictionaries and encodings) with one release swap, and frees the one it
//!   replaced at task boundaries through its limbo. A reader loads it with one acquire load.
//! - A record not in the mapped index table is a [`Spot`] in its tier's delta table: published
//!   by one compare-and-swap, moved (from a writer's segment to its pack once that is mapped,
//!   or by a settle) by one swap, the old spot freed at task boundaries.
//! - A value made whole to be read (decompressed, or put together from chunks) is published
//!   once into the [`Owned`] table and read in place from then on; the store swaps in a fresh
//!   table when the old one holds too much and frees the old one at task boundaries.
//! - What a read sets (verified, read) is a bit set with one relaxed `fetch_or`, and only when
//!   it is not set already, so a second read writes nothing.

use std::{borrow::Cow, ptr, sync::Arc};

use blaze_core::{FileName, Mapping};
use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_reclaim::Limbo;
use rustc_hash::FxHashMap;

use crate::{
    ByHash, Columns, Corrupt, Entry, Heat, Key, Kind, Ticket, Use, cold, header_schema, location,
    disk::{self, Form, Loc, NO_DICTIONARY, RECORD_HEADER, RecordHeader, SCHEMA_PREFIX, content_of},
    flags::{Bits, Flag, Row},
    index,
    segment::{ChunkRef, Segment},
    table::{Held, Keyed, Table},
};
use crate::sync::{
    OnceLock,
    atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
};

/// Writers a store can have: a ticket names its writer's segment by number.
pub(crate) const SEGMENTS_MOST: usize = 4096;

/// The most a chunked value reserves before its pieces are read: past it, it grows as they are.
const LIST_RESERVE: u64 = 64 << 20;

/// What the store and every reader and writer of it share.
pub(crate) struct Shared {
    view: AtomicPtr<View>,
    owned: AtomicPtr<Owned>,
    segments: Box<[OnceLock<Arc<Segment>>]>,
    /// The store's limbo once the store is dropped: what readers still holding this may read.
    orphans: AtomicPtr<Limbo>,
}

impl Shared {
    pub(crate) fn new(view: View) -> Shared {
        Shared {
            view: AtomicPtr::new(Box::into_raw(Box::new(view))),
            owned: AtomicPtr::new(Box::into_raw(Box::new(Owned::new()))),
            segments: (0..SEGMENTS_MOST).map(|_| OnceLock::new()).collect(),
            orphans: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub(crate) fn view<'a>(&'a self, _held: Held<'a>) -> &'a View {
        // SAFETY: never null; a view swapped out is freed only by the store's limbo, as `Held`
        // says, or with `self`.
        unsafe { &*self.view.load(Ordering::Acquire) }
    }

    pub(crate) fn owned<'a>(&'a self, _held: Held<'a>) -> &'a Owned {
        // SAFETY: as in `view`.
        unsafe { &*self.owned.load(Ordering::Acquire) }
    }

    /// Publishes `view` and gives back the one it replaced, for the store's limbo. Only the
    /// store calls it.
    pub(crate) fn publish(&self, view: View) -> Box<View> {
        let old = self.view.swap(Box::into_raw(Box::new(view)), Ordering::AcqRel);
        // SAFETY: a leaked box, unlinked just now by the one caller that swaps.
        unsafe { Box::from_raw(old) }
    }

    /// Starts a fresh table of values made whole and gives back the old one, for the store's
    /// limbo. Only the store calls it.
    pub(crate) fn fresh_owned(&self) -> Box<Owned> {
        let old = self.owned.swap(Box::into_raw(Box::new(Owned::new())), Ordering::AcqRel);
        // SAFETY: as in `publish`.
        unsafe { Box::from_raw(old) }
    }

    pub(crate) fn add_segment(&self, segment: Arc<Segment>) {
        let number = segment.number as usize;
        assert!(number < SEGMENTS_MOST, "a store of more than {SEGMENTS_MOST} writers");
        assert!(self.segments[number].set(segment).is_ok(), "segment {number} made twice");
    }

    /// Whether the put that got `ticket` is durable.
    pub(crate) fn durable(&self, ticket: Ticket) -> bool {
        let (segment, put) = ticket.parts();
        ticket == Ticket::DURABLE
            || self
                .segments
                .get(segment as usize)
                .and_then(OnceLock::get)
                .is_some_and(|it| it.durable() >= put)
    }

    /// Keeps a dropped store's limbo until the last reader or writer lets go.
    pub(crate) fn orphan(&self, limbo: Limbo) {
        if limbo.is_empty() {
            return;
        }
        let old = self.orphans.swap(Box::into_raw(Box::new(limbo)), Ordering::AcqRel);
        assert!(old.is_null(), "a store's limbo is orphaned once");
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: `view` is a leaked box that only this drop frees; no reader is left, since
        // each holds `self`.
        drop(unsafe { Box::from_raw(*self.view.get_mut()) });
        // SAFETY: as above, for `owned`.
        drop(unsafe { Box::from_raw(*self.owned.get_mut()) });
        let orphans = *self.orphans.get_mut();
        if !orphans.is_null() {
            // SAFETY: a leaked box (not null, checked) that `orphan` set once and only this drop
            // frees.
            drop(unsafe { Box::from_raw(orphans) });
        }
    }
}

/// The store as readers see it: immutable once published.
pub(crate) struct View {
    pub(crate) tiers: Vec<TierView>,
    pub(crate) columns: Arc<FxHashMap<SchemaHash, Arc<dyn Columns>>>,
}

pub(crate) struct TierView {
    pub(crate) dir: FileName,
    pub(crate) index_file: FileName,
    pub(crate) index: Option<Arc<Index>>,
    pub(crate) delta: Arc<Table<Spot>>,
    /// The delta an index swap left, while a writer may still put into it.
    pub(crate) prev: Option<Arc<Table<Spot>>>,
    pub(crate) packs: Vec<PackView>,
    pub(crate) externals: ByHash<ContentId, ExternalView>,
    pub(crate) dictionaries: Vec<(Kind, Result<Arc<cold::Prepared>, String>)>,
}

pub(crate) struct PackView {
    pub(crate) file: FileName,
    pub(crate) mapping: Option<Mapping>,
    /// How much of `mapping` is the store's.
    pub(crate) usable: u64,
}

impl PackView {
    fn bytes(&self, at: u64, len: u64) -> Option<&[u8]> {
        let end = at.checked_add(len).filter(|&end| end <= self.usable)?;
        self.mapping.as_ref()?.bytes().get(usize::try_from(at).ok()?..usize::try_from(end).ok()?)
    }
}

pub(crate) struct ExternalView {
    pub(crate) file: FileName,
    pub(crate) mapping: Option<Mapping>,
}

/// A tier's mapped index table, with its slots' flags.
pub(crate) struct Index {
    pub(crate) mapping: Mapping,
    shape: index::Shape,
    pub(crate) bits: Bits,
}

impl Index {
    pub(crate) fn new(mapping: Mapping) -> Result<Index, &'static str> {
        let table = index::Table::new(mapping.bytes())?;
        let (shape, bits) = (table.shape(), Bits::new(table.slots()));
        Ok(Index { mapping, shape, bits })
    }

    pub(crate) fn table(&self) -> index::Table<'_> {
        index::Table::again(self.mapping.bytes(), self.shape)
    }
}

/// Where a record of a delta is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Place {
    /// In a pack, under the bytes its mapping covers.
    Pack(Loc),
    /// In a writer's segment, not yet in a mapped pack.
    Segment(Seg),
}

impl Place {
    pub(crate) fn kind(self) -> Kind {
        match self {
            Place::Pack(loc) => loc.kind,
            Place::Segment(seg) => seg.kind,
        }
    }

    pub(crate) fn form(self) -> Form {
        match self {
            Place::Pack(loc) => loc.form,
            Place::Segment(seg) => seg.form,
        }
    }
}

/// A record in a writer's segment: `offset` is where its header starts in `chunk`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Seg {
    pub(crate) chunk: ChunkRef,
    pub(crate) segment: u32,
    pub(crate) offset: u64,
    pub(crate) kind: Kind,
    pub(crate) form: Form,
    pub(crate) stored: u64,
    pub(crate) raw: u64,
}

/// A record of a delta table.
pub(crate) struct Spot {
    pub(crate) key: Key,
    pub(crate) content: ContentId,
    pub(crate) place: Place,
    /// The put that made it, durable once its writer's segment is committed that far.
    pub(crate) ticket: Ticket,
    /// Its bytes were hashed and found to be what was recorded: a spot's place never changes.
    verified: AtomicBool,
}

impl Spot {
    pub(crate) fn new(key: Key, content: ContentId, place: Place, ticket: Ticket) -> Spot {
        Spot { key, content, place, ticket, verified: AtomicBool::new(false) }
    }

    /// It, at `place`.
    pub(crate) fn at(&self, place: Place) -> Box<Spot> {
        Box::new(Spot::new(self.key, self.content, place, self.ticket))
    }
}

impl Keyed for Spot {
    type Key = Key;

    fn key(&self) -> Key {
        self.key
    }

    fn hash(key: &Key) -> u64 {
        u64::from_le_bytes(*key.0.0.first_chunk().expect("a key is 32 bytes"))
    }
}

/// Values made whole to be read, by where their record is.
pub(crate) struct Owned {
    table: Table<Decoded>,
    bytes: AtomicU64,
}

struct Decoded {
    id: (u64, u64),
    content: ContentId,
    bytes: Box<[u8]>,
}

impl Keyed for Decoded {
    type Key = (u64, u64);

    fn key(&self) -> (u64, u64) {
        self.id
    }

    fn hash(&(a, b): &(u64, u64)) -> u64 {
        let mixed = (a ^ b.rotate_left(29)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        mixed ^ (mixed >> 29)
    }
}

impl Owned {
    fn new() -> Owned {
        Owned { table: Table::new(1024, 0), bytes: AtomicU64::new(0) }
    }

    /// How many bytes it holds.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    fn get<'a>(&'a self, held: Held<'a>, id: (u64, u64)) -> Option<(ContentId, &'a [u8])> {
        self.table.get(held, &id).map(|(_, it)| (it.content, &*it.bytes))
    }

    /// Publishes `bytes` as the value made whole at `id`, or gives the one published first.
    fn keep<'a>(&'a self, held: Held<'a>, id: (u64, u64), content: ContentId, bytes: Vec<u8>) -> &'a [u8] {
        let len = bytes.len() as u64;
        let decoded = Box::new(Decoded { id, content, bytes: bytes.into_boxed_slice() });
        match self.table.insert(held, decoded) {
            Ok((_, it)) => {
                self.bytes.fetch_add(len, Ordering::Relaxed);
                &it.bytes
            }
            Err((_, (_, it))) => &it.bytes,
        }
    }
}

/// A record found, in the view it was found in.
#[derive(Clone, Copy)]
pub(crate) struct Found<'a> {
    view: &'a View,
    pub(crate) tier: usize,
    pub(crate) place: Place,
    pub(crate) at: At<'a>,
}

/// Where a found record's flags are.
#[derive(Clone, Copy)]
pub(crate) enum At<'a> {
    Slot(&'a Index, u64),
    Spot(&'a Table<Spot>, u64, &'a Spot),
}

impl At<'_> {
    pub(crate) fn get(self, flag: Flag) -> bool {
        match self {
            At::Slot(index, slot) => index.bits.get(slot, flag),
            At::Spot(table, row, _) => table.flag(row, flag as usize),
        }
    }

    pub(crate) fn set(self, flag: Flag) {
        match self {
            At::Slot(index, slot) => index.bits.set(slot, flag),
            At::Spot(table, row, _) => table.set_flag(row, flag as usize),
        }
    }

    /// Its row, for the store: stable while it holds `&mut` of itself and swaps no table.
    pub(crate) fn row(self) -> Row {
        match self {
            At::Slot(_, slot) => Row::Slot(slot),
            At::Spot(_, row, _) => Row::Delta(row),
        }
    }

    fn verified(self) -> bool {
        match self {
            At::Slot(index, slot) => index.bits.get(slot, Flag::Verified),
            At::Spot(_, _, spot) => spot.verified.load(Ordering::Relaxed),
        }
    }

    fn set_verified(self) {
        match self {
            At::Slot(index, slot) => index.bits.set(slot, Flag::Verified),
            At::Spot(_, _, spot) => {
                if !spot.verified.load(Ordering::Relaxed) {
                    spot.verified.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

/// A read of the store, valid for `'a`.
#[derive(Clone, Copy)]
pub(crate) struct Look<'a> {
    pub(crate) shared: &'a Shared,
    pub(crate) held: Held<'a>,
}

impl<'a> Look<'a> {
    pub(crate) fn new(shared: &'a Shared, held: Held<'a>) -> Look<'a> {
        Look { shared, held }
    }

    pub(crate) fn view(self) -> &'a View {
        self.shared.view(self.held)
    }

    pub(crate) fn find(self, key: &Key) -> Option<Result<Found<'a>, Corrupt>> {
        let view = self.view();
        for (at, tier) in view.tiers.iter().enumerate() {
            for table in std::iter::once(&tier.delta).chain(&tier.prev) {
                if let Some((row, spot)) = table.get(self.held, key) {
                    return Some(Ok(Found { view, tier: at, place: spot.place, at: At::Spot(table, row, spot) }));
                }
            }
            let Some(index) = &tier.index else { continue };
            match index.table().get(key) {
                Some((slot, Ok(loc))) => {
                    return Some(Ok(Found { view, tier: at, place: Place::Pack(loc), at: At::Slot(index, slot) }));
                }
                Some((_, Err(what))) => {
                    return Some(Err(Corrupt { key: *key, pack: tier.index_file.clone(), what: what.into() }));
                }
                None => {}
            }
        }
        None
    }

    /// The content and ticket of the value kept under `key`, unverified, passing over a record
    /// whose header is not whole (a put shadows it).
    pub(crate) fn kept(self, key: &Key) -> Option<(ContentId, Ticket)> {
        let view = self.view();
        for (at, tier) in view.tiers.iter().enumerate() {
            for table in std::iter::once(&tier.delta).chain(&tier.prev) {
                if let Some((_, spot)) = table.get(self.held, key) {
                    return Some((spot.content, spot.ticket));
                }
            }
            let Some(index) = &tier.index else { continue };
            if let Some((slot, Ok(loc))) = index.table().get(key) {
                let found = Found { view, tier: at, place: Place::Pack(loc), at: At::Slot(index, slot) };
                if let Ok((header, _)) = self.record(found, key) {
                    return Some((header.content, Ticket::DURABLE));
                }
            }
        }
        None
    }

    /// Every record of tier `tier` a lookup finds, with where: its deltas' and its index
    /// table's they do not shadow. A slot that is not whole is in `bad`.
    pub(crate) fn rows(self, tier: usize, bad: &mut Vec<(Key, &'static str)>) -> Vec<(Key, Found<'a>)> {
        let view = self.view();
        let it = &view.tiers[tier];
        let mut out = Vec::new();
        let mut shadow = rustc_hash::FxHashSet::default();
        for table in std::iter::once(&it.delta).chain(&it.prev) {
            for (row, spot) in table.iter(self.held) {
                if shadow.insert(spot.key) {
                    out.push((spot.key, Found { view, tier, place: spot.place, at: At::Spot(table, row, spot) }));
                }
            }
        }
        if let Some(index) = &it.index {
            for (slot, key, loc) in index.table().iter() {
                match loc {
                    Ok(_) if shadow.contains(&key) => {}
                    Ok(loc) => out.push((key, Found { view, tier, place: Place::Pack(loc), at: At::Slot(index, slot) })),
                    Err(what) => bad.push((key, what)),
                }
            }
        }
        out
    }

    pub(crate) fn columns_of(self, bytes: &[u8]) -> Option<&'a Arc<dyn Columns>> {
        self.view().columns.get(&header_schema(bytes)?)
    }

    /// The newest dictionary of `kind` the own tier holds, by number.
    pub(crate) fn dictionary_of(self, kind: Kind) -> Option<(u32, &'a cold::Prepared)> {
        let dictionaries = &self.view().tiers[0].dictionaries;
        let at = dictionaries.iter().rposition(|(it, bytes)| *it == kind && bytes.is_ok())?;
        Some((at as u32, dictionaries[at].1.as_deref().expect("found by being whole")))
    }

    /// The encodings named by a columns payload, and the encoded bytes after the name.
    fn columns_in<'p>(self, payload: &'p [u8]) -> Result<(&'a dyn Columns, &'p [u8]), String> {
        let Some((schema, rest)) = payload.split_at_checked(SCHEMA_PREFIX) else {
            return Err("its payload is shorter than the schema it starts with".into());
        };
        let schema = SchemaHash(ContentId(schema.try_into().expect("a schema is 32 bytes")));
        match self.view().columns.get(&schema) {
            Some(columns) => Ok((columns.as_ref(), rest)),
            None => Err(format!(
                "it is kept in the columns of schema {:x}, and the store was given no encoding of it",
                schema.0
            )),
        }
    }

    /// The file a found record is named by when it is corrupt.
    fn file_of(self, found: Found<'a>) -> FileName {
        let tier = &found.view.tiers[found.tier];
        match found.place {
            Place::Pack(loc) => match tier.packs.get(loc.pack as usize) {
                Some(pack) => pack.file.clone(),
                None => tier.index_file.clone(),
            },
            Place::Segment(seg) => FileName::new(&format!("{}/segment-{}", tier.dir, seg.segment))
                .expect("a store's file names stay in its directory"),
        }
    }

    /// The bytes of a pack, in the view the record was found in or, for a record published
    /// after it, the one published since: a view never loses a mapping a found record names.
    fn pack_bytes(self, found: Found<'a>, loc: Loc, at: u64, len: u64) -> Option<&'a [u8]> {
        let of = |view: &'a View| view.tiers[found.tier].packs.get(loc.pack as usize)?.bytes(at, len);
        of(found.view).or_else(|| of(self.view()))
    }

    fn external(self, found: Found<'a>, content: &ContentId) -> Result<&'a [u8], Option<FileName>> {
        let of = |view: &'a View| view.tiers[found.tier].externals.get(content);
        let newer = || of(self.view());
        match of(found.view).filter(|it| it.mapping.is_some()).or_else(newer) {
            Some(ExternalView { mapping: Some(mapping), .. }) => Ok(mapping.bytes()),
            Some(ExternalView { file, mapping: None }) => Err(Some(file.clone())),
            None => Err(None),
        }
    }

    fn dictionary(self, found: Found<'a>, number: u32) -> Option<&'a Result<Arc<cold::Prepared>, String>> {
        let of = |view: &'a View| view.tiers[found.tier].dictionaries.get(number as usize).map(|(_, it)| it);
        of(found.view).or_else(|| of(self.view()))
    }

    /// The header of a found record, checked to be of `key`, and its payload.
    pub(crate) fn record(self, found: Found<'a>, key: &Key) -> Result<(RecordHeader, &'a [u8]), Corrupt> {
        let corrupt = |what: String| Corrupt { key: *key, pack: self.file_of(found), what };
        let (at, header, payload) = match found.place {
            Place::Pack(loc) => {
                let tier = &found.view.tiers[found.tier];
                if loc.pack as usize >= tier.packs.len() {
                    return Err(corrupt(format!("its index names pack {} its head does not have", loc.pack)));
                }
                let at = loc.offset;
                let header = self
                    .pack_bytes(found, loc, at, RECORD_HEADER)
                    .ok_or_else(|| corrupt(format!("its record at {at} is outside the pack")))?;
                let header =
                    RecordHeader::decode(header).map_err(|what| corrupt(format!("the record at {at}: {what}")))?;
                let payload = self
                    .pack_bytes(found, loc, at + RECORD_HEADER, header.stored)
                    .ok_or_else(|| corrupt(format!("the record at {at} runs past the pack")))?;
                (at, header, payload)
            }
            Place::Segment(seg) => {
                // SAFETY: a spot names a chunk that is freed only through the store's limbo
                // after the spot is unlinked, and `self.held` outlives what this returns.
                let chunk = unsafe { seg.chunk.get::<'a>() };
                let at = seg.offset;
                // SAFETY: `seg` is the place of a published spot, and these are the header and
                // payload of the record it names, which the writer wrote before it published the
                // spot (`Own::claim`, or `Own::keep` for the store's own) and never writes again.
                let bytes = unsafe { chunk.bytes(at, RECORD_HEADER + seg.stored) }
                    .ok_or_else(|| corrupt(format!("its record at {at} is outside its segment")))?;
                let (header, payload) = bytes.split_at(RECORD_HEADER as usize);
                let header =
                    RecordHeader::decode(header).map_err(|what| corrupt(format!("the record at {at}: {what}")))?;
                (at, header, payload)
            }
        };
        if header.key != *key {
            return Err(corrupt(format!("the record at {at} is of another key")));
        }
        Ok((header, payload))
    }

    /// The bytes of a found record, checked against its content hash: once a record for reads
    /// and moves, every time for [`Use::Check`].
    pub(crate) fn value(self, found: Found<'a>, key: &Key, why: Use) -> Result<(RecordHeader, Cow<'a, [u8]>), Corrupt> {
        let (header, payload) = self.record(found, key)?;
        let corrupt = |what: String| Corrupt { key: *key, pack: self.file_of(found), what };
        let at = match found.place {
            Place::Pack(loc) => loc.offset,
            Place::Segment(seg) => seg.offset,
        };
        let bytes: Cow<'a, [u8]> = match header.form {
            Form::Warm | Form::ColdColumns => {
                let (columns, encoded) = self.columns_in(payload).map_err(corrupt)?;
                let hot = match header.form {
                    Form::Warm => columns.hot_from_warm(encoded),
                    _ => columns.hot_from_cold(encoded),
                };
                Cow::Owned(hot.map_err(|why| corrupt(format!("its columns do not decode: {why}")))?)
            }
            Form::Raw => Cow::Borrowed(payload),
            Form::Zstd if header.dictionary == NO_DICTIONARY => {
                Cow::Owned(cold::decompress_long(payload, header.raw).map_err(corrupt)?)
            }
            Form::Lz4 => Cow::Owned(cold::unlz4(payload, header.raw).map_err(corrupt)?),
            Form::Zstd => {
                let dictionary = self.dictionary(found, header.dictionary).ok_or_else(|| {
                    corrupt(format!("it names dictionary {} the store does not have", header.dictionary))
                })?;
                let dictionary =
                    dictionary.as_deref().map_err(|why| corrupt(format!("its dictionary {why}")))?;
                Cow::Owned(cold::decompress(dictionary, payload, header.raw).map_err(corrupt)?)
            }
            Form::Chunked => {
                let chunks = disk::decode_chunks(payload).map_err(|what| corrupt(what.into()))?;
                let mut total = 0u64;
                for (chunk, len) in &chunks {
                    if *len > crate::chunk::MAX as u64 {
                        return Err(corrupt(format!("its chunk {} is {len} bytes, longer than a chunk", chunk.0)));
                    }
                    total = total.checked_add(*len).ok_or_else(|| corrupt("its chunks' lengths overflow".into()))?;
                }
                if total != header.raw {
                    return Err(corrupt(format!("its chunks are {total} bytes, not the {} recorded", header.raw)));
                }
                // Grown as pieces are read, at most `LIST_RESERVE` reserved once the first is:
                // one chunk may be listed any number of times, so the lengths, which nothing has
                // verified yet, bound nothing.
                let mut whole = Vec::new();
                for (chunk, len) in chunks {
                    let piece = self
                        .find(&chunk)
                        .ok_or_else(|| corrupt(format!("its chunk {} is not in the store", chunk.0)))??;
                    if !matches!(piece.place.form(), Form::Raw | Form::Lz4 | Form::Zstd) {
                        return Err(corrupt(format!("its chunk {} is not kept as a chunk", chunk.0)));
                    }
                    let (_, piece) = self.value(piece, &chunk, why)?;
                    if piece.len() as u64 != len {
                        return Err(corrupt(format!("its chunk {} is of another length", chunk.0)));
                    }
                    if whole.is_empty() {
                        whole.reserve_exact(piece.len().max(total.min(LIST_RESERVE) as usize));
                    }
                    whole.extend_from_slice(&piece);
                }
                Cow::Owned(whole)
            }
            Form::External => match found.place {
                Place::Segment(_) => Cow::Borrowed(payload),
                Place::Pack(_) => match self.external(found, &header.content) {
                    Ok(bytes) => Cow::Borrowed(bytes),
                    Err(Some(file)) => {
                        return Err(Corrupt { key: *key, pack: file, what: "it was not mapped".into() });
                    }
                    Err(None) => {
                        return Err(corrupt(format!("its file {:x} is not in the head", header.content)));
                    }
                },
            },
            Form::Dictionary => {
                return Err(corrupt(format!("the record at {at} is a dictionary, not a value")));
            }
        };
        if bytes.len() as u64 != header.raw {
            return Err(corrupt(format!("it is {} bytes, not the {} recorded", bytes.len(), header.raw)));
        }
        let memo = !matches!(why, Use::Check);
        if !(memo && found.at.verified()) {
            let hashed = content_of(&bytes);
            if hashed != header.content {
                return Err(corrupt(format!("its bytes hash to {hashed:x}, not the {:x} recorded", header.content)));
            }
            if memo {
                found.at.set_verified();
            }
        }
        if matches!(why, Use::Read) {
            self.was_read(found);
        }
        Ok((header, bytes))
    }

    fn was_read(self, found: Found<'a>) {
        if found.tier == 0 {
            found.at.set(Flag::Read);
        }
    }

    /// Where a value made whole from a found record is kept in [`Owned`].
    fn owned_id(found: Found<'a>) -> (u64, u64) {
        match found.place {
            Place::Pack(loc) => (location(found.tier, loc.pack, loc.offset), 0),
            Place::Segment(seg) => {
                // SAFETY: as in `record`.
                let chunk = unsafe { seg.chunk.get::<'a>() };
                ((1 << 63) | u64::from(seg.segment), chunk.start + seg.offset)
            }
        }
    }

    pub(crate) fn read(self, found: Found<'a>, key: &Key) -> Result<Entry<'a>, Corrupt> {
        let id = Look::owned_id(found);
        let owned = self.shared.owned(self.held);
        if matches!(found.place.form(), Form::Zstd | Form::Chunked | Form::Lz4 | Form::Warm | Form::ColdColumns)
            && let Some((content, bytes)) = owned.get(self.held, id)
        {
            self.was_read(found);
            return Ok(Entry { key: *key, kind: found.place.kind(), content, bytes });
        }
        let (header, bytes) = self.value(found, key, Use::Read)?;
        let bytes = match bytes {
            Cow::Borrowed(bytes) => bytes,
            Cow::Owned(bytes) => owned.keep(self.held, id, header.content, bytes),
        };
        Ok(Entry { key: *key, kind: header.kind, content: header.content, bytes })
    }

    pub(crate) fn get(self, key: &Key) -> Result<Option<Entry<'a>>, Corrupt> {
        let Some(found) = self.find(key) else { return Ok(None) };
        self.read(found?, key).map(Some)
    }

    pub(crate) fn get_in_place(self, key: &Key) -> Result<Option<(Heat, Entry<'a>)>, Corrupt> {
        let Some(found) = self.find(key) else { return Ok(None) };
        let found = found?;
        if found.place.form() != Form::Warm {
            return self.read(found, key).map(|entry| Some((Heat::Hot, entry)));
        }
        let (header, payload) = self.record(found, key)?;
        if found.at.verified() {
            self.was_read(found);
        } else {
            self.value(found, key, Use::Read)?;
        }
        let warm = payload.get(SCHEMA_PREFIX..).ok_or_else(|| Corrupt {
            key: *key,
            pack: self.file_of(found),
            what: "its payload is shorter than the schema it starts with".into(),
        })?;
        Ok(Some((Heat::Warm, Entry { key: *key, kind: header.kind, content: header.content, bytes: warm })))
    }

    pub(crate) fn has(self, key: &Key) -> bool {
        self.find(key).is_some()
    }
}
