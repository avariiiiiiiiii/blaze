//! Per-worker write segments (docs/v2/plan/design/concurrency.md "Writes").
//!
//! A worker puts through a [`Writer`] of its own: it encodes the value, writes the record into
//! its own segment's memory and publishes the record's key into the shared delta table with
//! one compare-and-swap. No lock, and no shared write but that slot. The store drains every
//! segment when a commit begins: it copies what each published into its packs, and once those
//! are mapped it points each key at its pack with one swap, after which the segment's memory
//! is freed at task boundaries.
//!
//! A segment is a list of chunks, each written by its one writer past what it has published
//! and read by everyone below it. A record in a chunk is a 16-byte frame (its unit, whether it
//! is a copy) then the record as a pack holds it, except that a value that is a file of its
//! own carries its bytes until the store writes the file.

use std::{
    ptr::{self, NonNull},
    sync::Arc,
};

use blaze_ids::ContentId;
use blaze_reclaim::Guard;

use crate::{
    CHUNK_FROM, EXTERNAL_FROM, Kind, Key, Mismatch, Ticket, Unit, chunk, cold, cold_columns,
    disk::{self, CHUNK_DOMAIN, Form, NO_DICTIONARY, RECORD_HEADER, RecordHeader, content_of},
    flags::Flag,
    read::{Look, Place, Seg, Shared, Spot},
    table::Held,
};
use crate::sync::{
    OnceLock,
    atomic::{AtomicPtr, AtomicU64, Ordering},
    track,
};

/// A chunk's length, unless a record needs more.
#[cfg(not(test))]
pub(crate) const CHUNK: usize = 1 << 20;
#[cfg(test)]
pub(crate) const CHUNK: usize = 8 << 10;

/// What a record in a segment starts with: its unit (4 bytes), whether it is a copy (1 byte),
/// zeros to 16.
pub(crate) const FRAME: u64 = 16;

/// A frame's copy byte for a record no key points at: a chunk of an output another writer has
/// put and not yet made durable, written again so this writer's put does not wait on that
/// one's. A chunk's key is the hash of its bytes, so the two are the same value.
pub(crate) const COPY: u8 = 1;

/// A chunk's published word: bytes below `1 << BYTES_BITS`, puts ended in it above.
const BYTES_BITS: u32 = 40;

pub(crate) struct Chunk {
    bytes: NonNull<u8>,
    cap: usize,
    published: AtomicU64,
    next: AtomicPtr<Chunk>,
    /// Where it starts among every byte its segment has written: a place is unique by it.
    pub(crate) start: u64,
}

// SAFETY: a chunk's bytes are written only by its segment's one writer, at or past what it has
// kept; others read only bytes a published place names or that were published (both with
// release after the bytes were written), and those are never written again. The bytes are
// freed only with the chunk.
unsafe impl Send for Chunk {}
// SAFETY: as for `Send`: through `&Chunk` other threads read only bytes a published place names
// or that were published, never written again, and the atomics; the writer writes only through
// `unsafe fn write` and `zero`, whose callers write past what was kept.
unsafe impl Sync for Chunk {}

impl Chunk {
    fn new(cap: usize, start: u64) -> ChunkRef {
        let bytes: Box<[u8]> = vec![0; cap].into_boxed_slice();
        let bytes = NonNull::new(Box::into_raw(bytes).cast::<u8>()).expect("a box is not null");
        track::write(bytes.as_ptr(), cap);
        let chunk = Chunk { bytes, cap, published: AtomicU64::new(0), next: AtomicPtr::new(ptr::null_mut()), start };
        ChunkRef(NonNull::from(Box::leak(Box::new(chunk))))
    }

    pub(crate) fn cap(&self) -> u64 {
        self.cap as u64
    }

    /// The bytes at `at`; `None` past the chunk.
    ///
    /// # Safety
    ///
    /// The range is one a published place names, or below what the chunk published: the
    /// writer may be writing any other bytes of it.
    pub(crate) unsafe fn bytes(&self, at: u64, len: u64) -> Option<&[u8]> {
        let end = at.checked_add(len)?;
        if end > self.cap as u64 {
            return None;
        }
        // SAFETY: `at + len <= cap` (checked above), so `at` is inside the allocation of `cap`
        // bytes.
        let start = unsafe { self.bytes.as_ptr().add(at as usize) };
        track::read(start, len as usize);
        // SAFETY: the `len` bytes from `start` are inside the allocation, which lives as long as
        // the chunk; bytes a place names or that were published are not written again (the
        // caller's contract).
        Some(unsafe { std::slice::from_raw_parts(start, len as usize) })
    }

    /// Writes `data` at `at`.
    ///
    /// # Safety
    ///
    /// Only the segment's writer calls it, at or past what it has kept, where no place points
    /// and nothing was published.
    unsafe fn write(&self, at: usize, data: &[u8]) {
        assert!(at + data.len() <= self.cap, "a write past its chunk");
        let from = data.as_ptr();
        // SAFETY: `at + data.len() <= cap` (asserted above), so `at` is inside the allocation.
        let to = unsafe { self.bytes.as_ptr().add(at) };
        track::write(to, data.len());
        // SAFETY: `to` has `data.len()` writable bytes in the allocation, which `data` (the
        // caller's own buffer) does not overlap, and no one else reads or writes them (the
        // caller's contract).
        unsafe { ptr::copy_nonoverlapping(from, to, data.len()) }
    }

    /// Writes `len` zeros at `at`.
    ///
    /// # Safety
    ///
    /// As `write`.
    unsafe fn zero(&self, at: usize, len: usize) {
        assert!(at + len <= self.cap, "a write past its chunk");
        // SAFETY: `at + len <= cap` (asserted above), so `at` is inside the allocation.
        let to = unsafe { self.bytes.as_ptr().add(at) };
        track::write(to, len);
        // SAFETY: `to` has `len` writable bytes in the allocation, and no one else reads or
        // writes them (the caller's contract, as in `write`).
        unsafe { ptr::write_bytes(to, 0, len) }
    }

    /// Bytes of it its writer has published, and how many puts ended in it.
    pub(crate) fn published(&self) -> (u64, u64) {
        let word = self.published.load(Ordering::Acquire);
        (word & ((1 << BYTES_BITS) - 1), word >> BYTES_BITS)
    }

    fn publish(&self, bytes: u64, puts: u64) {
        assert!(
            bytes < 1 << BYTES_BITS && puts < 1 << (64 - BYTES_BITS),
            "a chunk of 2^40 bytes or 2^24 puts"
        );
        self.published.store(bytes | (puts << BYTES_BITS), Ordering::Release);
    }

    /// The chunk after it, once its writer has moved on: then what it published is all of it.
    pub(crate) fn next(&self) -> Option<ChunkRef> {
        NonNull::new(self.next.load(Ordering::Acquire)).map(ChunkRef)
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        track::free(self.bytes.as_ptr(), self.cap);
        // SAFETY: made from a boxed slice of `cap` bytes in `Chunk::new`, freed only here.
        drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(self.bytes.as_ptr(), self.cap)) });
    }
}

/// A chunk of a segment, by address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ChunkRef(NonNull<Chunk>);

// SAFETY: a chunk is `Sync`; a reference is dereferenced only while its chunk is alive (see
// `ChunkRef::get`).
unsafe impl Send for ChunkRef {}
// SAFETY: a `ChunkRef` is an address: sharing it shares nothing until `ChunkRef::get`, whose
// caller promises the chunk is alive, and `Chunk` is `Sync`.
unsafe impl Sync for ChunkRef {}

impl ChunkRef {
    /// The chunk it names.
    ///
    /// # Safety
    ///
    /// The chunk must live for `'a`: it is the writer's tail, or not yet unlinked by the store,
    /// or named by a spot the caller found under a `Held` of `'a` (a chunk is freed only
    /// through the store's limbo, after every spot into it was unlinked).
    pub(crate) unsafe fn get<'a>(self) -> &'a Chunk {
        // SAFETY: alive, by the caller's contract.
        unsafe { self.0.as_ref() }
    }
}

/// One writer's segment, as the store and the writer share it.
pub(crate) struct Segment {
    pub(crate) number: u32,
    /// The oldest chunk not freed: the store moves it on as it frees chunks.
    first: AtomicPtr<Chunk>,
    /// How many of its puts are durable.
    durable: AtomicU64,
    /// Why a put of it could not be kept: zstd refused the value. The store stops on it.
    failed: OnceLock<String>,
}

impl Segment {
    pub(crate) fn first(&self) -> ChunkRef {
        ChunkRef(NonNull::new(self.first.load(Ordering::Acquire)).expect("a segment has a chunk"))
    }

    pub(crate) fn durable(&self) -> u64 {
        self.durable.load(Ordering::Acquire)
    }

    pub(crate) fn set_durable(&self, puts: u64) {
        self.durable.store(puts, Ordering::Release);
    }

    pub(crate) fn failed(&self) -> Option<&str> {
        self.failed.get().map(String::as_str)
    }

    /// Unlinks its oldest chunk, `chunk`, which its writer has moved on from and no place names
    /// any more, for the caller to hold in its limbo. Only the store calls it.
    pub(crate) fn unlink_first(&self, chunk: ChunkRef) -> Box<Chunk> {
        assert_eq!(self.first(), chunk, "a segment's chunks are freed oldest first");
        // SAFETY: the first chunk, which only this call frees.
        let next = unsafe { chunk.get() }.next().expect("the chunk a writer writes is not freed");
        self.first.store(next.0.as_ptr(), Ordering::Release);
        // SAFETY: a leaked box from `Chunk::new`, unlinked just now and owned by no one else
        // (the segment's drop starts at `first`).
        unsafe { Box::from_raw(chunk.0.as_ptr()) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        let mut at = *self.first.get_mut();
        while !at.is_null() {
            // SAFETY: the chunks from `first` on are this segment's leaked boxes, and nothing
            // reads a segment being dropped: its writer and the store let go of it.
            let chunk = unsafe { Box::from_raw(at) };
            at = chunk.next.load(Ordering::Acquire);
        }
    }
}

/// A worker's way to put values into the store: one a worker, from [`crate::Store::writer`],
/// used from a task of any thread.
///
/// A put is readable by every reader at once and durable once the store has committed it:
/// [`Writer::durable`] says when.
pub struct Writer {
    pub(crate) shared: Arc<Shared>,
    pub(crate) own: Own,
}

impl Writer {
    /// [`Writer::put_in`] of [`Unit::SHARED`].
    pub fn put(&mut self, guard: &Guard, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<Ticket, Mismatch> {
        self.put_in(guard, Unit::SHARED, key, kind, bytes)
    }

    /// Keeps `bytes` under `key` in the packs of `unit`, as [`crate::Store::put_in`] does.
    pub fn put_in(
        &mut self,
        guard: &Guard,
        unit: Unit,
        key: Key,
        kind: Kind,
        bytes: Vec<u8>,
    ) -> Result<Ticket, Mismatch> {
        let content = content_of(&bytes);
        self.own.put(Look::new(&self.shared, Held::task(guard)), unit, key, kind, content, &bytes)
    }

    /// Whether the put that got `ticket` (from any writer of the store) is durable.
    pub fn durable(&self, ticket: Ticket) -> bool {
        self.shared.durable(ticket)
    }
}

/// The writing end of a segment.
pub(crate) struct Own {
    segment: Arc<Segment>,
    tail: ChunkRef,
    /// Bytes of `tail` kept: published, or of the put being made.
    at: u64,
    /// Puts begun.
    puts: u64,
    /// Puts that ended in `tail`.
    puts_here: u64,
}

impl Own {
    pub(crate) fn new(number: u32) -> Own {
        let tail = Chunk::new(CHUNK, 0);
        let segment = Arc::new(Segment {
            number,
            first: AtomicPtr::new(tail.0.as_ptr()),
            durable: AtomicU64::new(0),
            failed: OnceLock::new(),
        });
        Own { segment, tail, at: 0, puts: 0, puts_here: 0 }
    }

    pub(crate) fn segment(&self) -> &Arc<Segment> {
        &self.segment
    }

    /// Keeps `bytes`, whose hash is `content`, under `key`.
    pub(crate) fn put(
        &mut self,
        look: Look<'_>,
        unit: Unit,
        key: Key,
        kind: Kind,
        content: ContentId,
        bytes: &[u8],
    ) -> Result<Ticket, Mismatch> {
        if let Some((kept, ticket)) = look.kept(&key) {
            return match kept == content {
                true => Ok(ticket),
                false => Err(Mismatch { key, kept, given: content }),
            };
        }
        let ticket = Ticket::of(self.segment.number, self.puts + 1);
        let raw = bytes.len() as u64;
        let record = |form, dictionary, stored: &[u8]| RecordHeader {
            kind,
            form,
            dictionary,
            stored: stored.len() as u64,
            raw,
            key,
            content,
        };
        let won = if kind == Kind::Output && bytes.len() >= CHUNK_FROM {
            let mut refs = Vec::new();
            for range in chunk::chunks(bytes) {
                let piece = &bytes[range];
                let chunk = Key(ContentId::of(CHUNK_DOMAIN, piece));
                let kept = look.kept(&chunk);
                let copy = kept.is_some_and(|(_, there)| {
                    there.segment() != self.segment.number && !look.shared.durable(there)
                });
                if kept.is_none() || copy {
                    let (form, stored) = match cold::lz4(piece) {
                        Some(stored) => (Form::Lz4, stored),
                        None => (Form::Raw, piece.to_vec()),
                    };
                    let header = RecordHeader {
                        kind: Kind::Output,
                        form,
                        dictionary: 0,
                        stored: stored.len() as u64,
                        raw: piece.len() as u64,
                        key: chunk,
                        content: content_of(piece),
                    };
                    match copy {
                        true => self.write_copy(Unit::SHARED, &header, &stored),
                        // A chunk another put kept meanwhile is the same bytes: either is it.
                        false => _ = self.claim(look, Unit::SHARED, header, &stored, ticket),
                    }
                }
                refs.push((chunk, piece.len() as u64));
            }
            let payload = disk::encode_chunks(&refs);
            self.claim(look, unit, record(Form::Chunked, 0, &payload), &payload, ticket)
        } else if !kind.cold() && kind != Kind::Output && bytes.len() >= EXTERNAL_FROM {
            // Its bytes ride in the segment; the store writes them as the file at the commit.
            self.claim(look, unit, record(Form::External, 0, bytes), bytes, ticket)
        } else if let Some(columns) = look.columns_of(bytes).filter(|_| kind.cold()) {
            match cold_columns(columns.as_ref(), bytes) {
                Ok(payload) => self.claim(look, unit, record(Form::ColdColumns, 0, &payload), &payload, ticket),
                Err(why) => return Ok(self.fail(kind, why)),
            }
        } else if kind.cold() && bytes.len() >= cold::LONG_FROM {
            match cold::compress_long(bytes) {
                Ok(stored) => self.claim(look, unit, record(Form::Zstd, NO_DICTIONARY, &stored), &stored, ticket),
                Err(why) => return Ok(self.fail(kind, why)),
            }
        } else if kind.cold()
            && let Some((number, dictionary)) = look.dictionary_of(kind)
        {
            match cold::compress(dictionary, bytes) {
                Ok(stored) => self.claim(look, unit, record(Form::Zstd, number, &stored), &stored, ticket),
                Err(why) => return Ok(self.fail(kind, why)),
            }
        } else {
            // A cold kind with no dictionary yet is kept raw and sampled, and its dictionary
            // trained on the samples.
            self.claim(look, unit, record(Form::Raw, 0, bytes), bytes, ticket)
        };
        self.end_put();
        match won {
            Ok(()) => Ok(ticket),
            Err((kept, there)) if kept == content => Ok(there),
            Err((kept, _)) => Err(Mismatch { key, kept, given: content }),
        }
    }

    /// zstd refusing a value is a broken build of it (a missing feature), not something a
    /// value can cause: the store stops on it rather than keep the value another way. The put
    /// still takes its ticket, which is never durable.
    fn fail(&mut self, kind: Kind, why: String) -> Ticket {
        _ = self.segment.failed.set(format!("compressing a {} value: {why}", kind.name()));
        let ticket = Ticket::of(self.segment.number, self.puts + 1);
        self.end_put();
        ticket
    }

    /// Writes a record and publishes its key; when the key was published meanwhile, keeps
    /// nothing and gives back the content and ticket of what is there.
    fn claim(
        &mut self,
        look: Look<'_>,
        unit: Unit,
        header: RecordHeader,
        payload: &[u8],
        ticket: Ticket,
    ) -> Result<(), (ContentId, Ticket)> {
        let (seg, span) = self.write(unit, &header, payload, 0);
        let spot = Box::new(Spot::new(header.key, header.content, Place::Segment(seg), ticket));
        let delta = &look.view().tiers[0].delta;
        match delta.insert(look.held, spot) {
            Ok((row, _)) => {
                delta.set_flag(row, Flag::New as usize);
                self.at += span;
                Ok(())
            }
            Err((_, (_, there))) => Err((there.content, there.ticket)),
        }
    }

    /// Writes a record no key will point at, kept for its bytes (see [`COPY`]).
    fn write_copy(&mut self, unit: Unit, header: &RecordHeader, payload: &[u8]) {
        let (_, span) = self.write(unit, header, payload, COPY);
        self.at += span;
    }

    /// Writes a record at the writer's next bytes, not yet kept: gives its place and length.
    pub(crate) fn write(&mut self, unit: Unit, header: &RecordHeader, payload: &[u8], copy: u8) -> (Seg, u64) {
        assert_eq!(payload.len() as u64, header.stored, "a record's payload is its stored length");
        let span = FRAME + header.span();
        if self.at + span > self.tail().cap() {
            self.roll(span);
        }
        let at = self.at as usize;
        let tail = self.tail();
        let mut frame = [0u8; FRAME as usize];
        frame[..4].copy_from_slice(&unit.0.to_le_bytes());
        frame[4] = copy;
        let body = at + FRAME as usize;
        let end = body + RECORD_HEADER as usize + payload.len();
        // SAFETY: this is the segment's one writer (`&mut self`), and `at` is past what it has
        // kept, so no place points into `at..at + span` and nothing there was published; `roll`
        // left room for `span` bytes.
        unsafe { tail.write(at, &frame) };
        // SAFETY: as above, for the header after the frame.
        unsafe { tail.write(body, &header.encode()) };
        // SAFETY: as above, for the payload after the header.
        unsafe { tail.write(body + RECORD_HEADER as usize, payload) };
        // SAFETY: as above, for the zeros up to `at + span`.
        unsafe { tail.zero(end, at + span as usize - end) };
        let seg = Seg {
            chunk: self.tail,
            segment: self.segment.number,
            offset: self.at + FRAME,
            kind: header.kind,
            form: header.form,
            stored: header.stored,
            raw: header.raw,
        };
        (seg, span)
    }

    /// Keeps and publishes the record [`Own::write`] wrote last, `span` bytes: for the store's
    /// own records (moved by a settle), whose keys it points at itself.
    pub(crate) fn keep(&mut self, span: u64) {
        self.at += span;
        self.tail().publish(self.at, self.puts_here);
    }

    fn tail(&self) -> &Chunk {
        // SAFETY: the tail has no next, and the store frees only chunks with a next.
        unsafe { self.tail.get() }
    }

    /// Seals the tail with what it holds and moves on to a chunk with room for `need` bytes.
    fn roll(&mut self, need: u64) {
        let old = self.tail();
        let next = Chunk::new((need as usize).max(CHUNK), old.start + old.cap());
        old.publish(self.at, self.puts_here);
        old.next.store(next.0.as_ptr(), Ordering::Release);
        self.tail = next;
        self.at = 0;
        self.puts_here = 0;
    }

    fn end_put(&mut self) {
        self.puts += 1;
        self.puts_here += 1;
        self.tail().publish(self.at, self.puts_here);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_publishes_its_bytes_and_puts_in_one_word() {
        let chunk = Chunk::new(4096, 0);
        // SAFETY: made just now and freed below.
        let it = unsafe { chunk.get() };
        assert_eq!(it.published(), (0, 0));
        it.publish(4000, 3);
        assert_eq!(it.published(), (4000, 3));
        // SAFETY: a leaked box no one else holds.
        drop(unsafe { Box::from_raw(chunk.0.as_ptr()) });
    }
}
