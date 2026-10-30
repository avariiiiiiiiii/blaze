//! Per-worker write segments (docs/v2/plan/design/concurrency.md "Writes").
//!
//! A worker puts through a [`Writer`] of its own: it encodes the value, writes the record into
//! its own segment's memory and publishes the record's key into the shared delta table with
//! one compare-and-swap. No lock and no write another worker reads, but for that slot. The
//! store drains every segment at a commit (`Store::flush`): it copies what each published into
//! its packs, and once those are mapped it points each key at its pack with one swap, after
//! which the segment's memory is freed at task boundaries.
//!
//! A segment is a list of chunks, each written by its one writer past what it has published
//! and read by everyone below it. A record in a chunk is a 16-byte frame (its unit, whether it
//! is a copy) then the record as a pack holds it, except that a file of its own carries its
//! bytes until the store writes the file.

use std::{
    ptr::{self, NonNull},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicPtr, AtomicU64, Ordering},
    },
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

/// A chunk's length, unless a record needs more.
#[cfg(not(test))]
pub(crate) const CHUNK: usize = 1 << 20;
#[cfg(test)]
pub(crate) const CHUNK: usize = 8 << 10;

/// What a record in a segment starts with: its unit (4 bytes), whether it is a copy (1 byte),
/// zeros to 16.
pub(crate) const FRAME: u64 = 16;

/// A frame's copy byte for a record its writer did not publish the key of: a chunk another
/// writer has put and not yet made durable, written again so this writer's put does not wait
/// on that one. A chunk's key is the hash of its bytes, so two copies are the same value.
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
// published; others read only bytes a place names, and a place is published (with release)
// after its bytes were written and never names bytes written again. The bytes are freed only
// with the chunk, which the store does through its limbo once no place names it.
unsafe impl Send for Chunk {}
unsafe impl Sync for Chunk {}

impl Chunk {
    fn new(cap: usize, start: u64) -> NonNull<Chunk> {
        let bytes: Box<[u8]> = vec![0; cap].into_boxed_slice();
        let bytes = NonNull::new(Box::into_raw(bytes).cast::<u8>()).expect("a box is not null");
        let chunk = Chunk { bytes, cap, published: AtomicU64::new(0), next: AtomicPtr::new(ptr::null_mut()), start };
        NonNull::from(Box::leak(Box::new(chunk)))
    }

    pub(crate) fn cap(&self) -> u64 {
        self.cap as u64
    }

    /// The bytes at `at`; `None` past the chunk. Callers read only what a place names or what
    /// was published.
    pub(crate) fn bytes(&self, at: u64, len: u64) -> Option<&[u8]> {
        let end = at.checked_add(len)?;
        if end > self.cap as u64 {
            return None;
        }
        // SAFETY: inside the allocation; bytes a place names are not written again.
        Some(unsafe { std::slice::from_raw_parts(self.bytes.as_ptr().add(at as usize), len as usize) })
    }

    /// Writes `data` at `at`.
    ///
    /// SAFETY: only the segment's writer calls it, at or past what it has published, where no
    /// place points.
    unsafe fn write(&self, at: usize, data: &[u8]) {
        assert!(at + data.len() <= self.cap, "a write past its chunk");
        // SAFETY: inside the allocation, and no reader reads there (the caller's contract).
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), self.bytes.as_ptr().add(at), data.len()) }
    }

    /// SAFETY: as `write`.
    unsafe fn zero(&self, at: usize, len: usize) {
        assert!(at + len <= self.cap, "a write past its chunk");
        // SAFETY: as in `write`.
        unsafe { ptr::write_bytes(self.bytes.as_ptr().add(at), 0, len) }
    }

    /// Bytes of it its writer has published, and how many puts ended in it.
    pub(crate) fn published(&self) -> (u64, u64) {
        let word = self.published.load(Ordering::Acquire);
        (word & ((1 << BYTES_BITS) - 1), word >> BYTES_BITS)
    }

    fn publish(&self, bytes: u64, puts: u64) {
        assert!(bytes < 1 << BYTES_BITS && puts < 1 << (64 - BYTES_BITS), "a chunk of more than 2^40 bytes or 2^24 puts");
        self.published.store(bytes | (puts << BYTES_BITS), Ordering::Release);
    }

    /// The chunk after it, once its writer has moved on: then what it published is all of it.
    pub(crate) fn next(&self) -> Option<NonNull<Chunk>> {
        NonNull::new(self.next.load(Ordering::Acquire))
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        // SAFETY: made from a boxed slice of `cap` bytes in `Chunk::new`, freed only here.
        drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(self.bytes.as_ptr(), self.cap)) });
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
    fn new(number: u32) -> (Arc<Segment>, NonNull<Chunk>) {
        let chunk = Chunk::new(CHUNK, 0);
        let segment = Segment {
            number,
            first: AtomicPtr::new(chunk.as_ptr()),
            durable: AtomicU64::new(0),
            failed: OnceLock::new(),
        };
        (Arc::new(segment), chunk)
    }

    pub(crate) fn first(&self) -> NonNull<Chunk> {
        NonNull::new(self.first.load(Ordering::Acquire)).expect("a segment has a chunk")
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
    pub(crate) fn unlink_first(&self, chunk: NonNull<Chunk>) -> Box<Chunk> {
        assert_eq!(self.first.load(Ordering::Acquire), chunk.as_ptr(), "a segment's chunks are freed oldest first");
        // SAFETY: `chunk` is this segment's first, which only this call frees.
        let next = unsafe { chunk.as_ref() }.next().expect("the chunk a writer writes is not freed");
        self.first.store(next.as_ptr(), Ordering::Release);
        // SAFETY: made by `Chunk::new` (a leaked box), unlinked just now, and owned by no one
        // else: the segment's drop starts at `first`.
        unsafe { Box::from_raw(chunk.as_ptr()) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        let mut at = *self.first.get_mut();
        while !at.is_null() {
            // SAFETY: the chunks from `first` on are this segment's, leaked boxes, and nothing
            // reads a segment being dropped (every writer and the store let go of it).
            let chunk = unsafe { Box::from_raw(at) };
            at = chunk.next.load(Ordering::Acquire);
        }
    }
}

/// A worker's way to put values into the store: one a worker, made once with
/// [`crate::Store::writer`], used on any thread but by one at a time.
///
/// A put is readable by every reader at once and durable once the store has committed it:
/// [`Writer::durable`] says when.
pub struct Writer {
    shared: Arc<Shared>,
    segment: Arc<Segment>,
    tail: NonNull<Chunk>,
    /// Bytes of `tail` kept: published, or of the put being made.
    at: u64,
    /// Puts begun.
    puts: u64,
    /// Puts that ended in `tail`.
    puts_here: u64,
}

// SAFETY: `tail` is a chunk of the writer's own segment, which `segment` keeps alive and which
// only this writer writes; a `&mut` of the writer is needed to write, so one thread at a time.
unsafe impl Send for Writer {}

impl Writer {
    pub(crate) fn new(shared: Arc<Shared>, number: u32) -> (Writer, Arc<Segment>) {
        let (segment, tail) = Segment::new(number);
        let writer = Writer { shared, segment: segment.clone(), tail, at: 0, puts: 0, puts_here: 0 };
        (writer, segment)
    }

    /// [`Writer::put_in`] of [`Unit::SHARED`].
    pub fn put(&mut self, guard: &Guard, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<Ticket, Mismatch> {
        self.put_in(guard, Unit::SHARED, key, kind, bytes)
    }

    /// Keeps `bytes` under `key` in the packs of `unit`, as [`crate::Store::put_in`] does,
    /// from a task (`guard`) of any thread.
    pub fn put_in(
        &mut self,
        guard: &Guard,
        unit: Unit,
        key: Key,
        kind: Kind,
        bytes: Vec<u8>,
    ) -> Result<Ticket, Mismatch> {
        let shared = Arc::clone(&self.shared);
        self.put_with(Look::new(&shared, Held::task(guard)), unit, key, kind, bytes)
    }

    /// Whether the put that got `ticket` (from any writer of the store) is durable.
    pub fn durable(&self, ticket: Ticket) -> bool {
        self.shared.durable(ticket)
    }

    pub(crate) fn put_with(
        &mut self,
        look: Look<'_>,
        unit: Unit,
        key: Key,
        kind: Kind,
        bytes: Vec<u8>,
    ) -> Result<Ticket, Mismatch> {
        let content = content_of(&bytes);
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
            for range in chunk::chunks(&bytes) {
                let piece = &bytes[range];
                let chunk = Key(ContentId::of(CHUNK_DOMAIN, piece));
                let copy = match look.kept_spot(&chunk) {
                    None => false,
                    Some(ticket) => !self.shared.durable(ticket) && ticket.segment() != self.segment.number,
                };
                if copy || look.kept(&chunk).is_none() {
                    let piece_content = content_of(piece);
                    let len = piece.len() as u64;
                    let header = |form, stored: &[u8]| RecordHeader {
                        kind: Kind::Output,
                        form,
                        dictionary: 0,
                        stored: stored.len() as u64,
                        raw: len,
                        key: chunk,
                        content: piece_content,
                    };
                    let (form, stored) = match cold::lz4(piece) {
                        Some(stored) => (Form::Lz4, stored),
                        None => (Form::Raw, piece.to_vec()),
                    };
                    let header = header(form, &stored);
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
            self.claim(look, unit, record(Form::External, 0, &bytes), &bytes, ticket)
        } else if let Some(columns) = look.columns_of(&bytes).filter(|_| kind.cold()) {
            match cold_columns(columns.as_ref(), &bytes) {
                Ok(payload) => self.claim(look, unit, record(Form::ColdColumns, 0, &payload), &payload, ticket),
                Err(why) => return Ok(self.fail(kind, why)),
            }
        } else if kind.cold() && bytes.len() >= cold::LONG_FROM {
            match cold::compress_long(&bytes) {
                Ok(stored) => self.claim(look, unit, record(Form::Zstd, NO_DICTIONARY, &stored), &stored, ticket),
                Err(why) => return Ok(self.fail(kind, why)),
            }
        } else if kind.cold()
            && let Some((number, dictionary)) = look.dictionary_of(kind)
        {
            match cold::compress(dictionary, &bytes) {
                Ok(stored) => self.claim(look, unit, record(Form::Zstd, number, &stored), &stored, ticket),
                Err(why) => return Ok(self.fail(kind, why)),
            }
        } else {
            // A cold kind with no dictionary yet is kept raw; the store samples it when it
            // drains the segment, and trains the kind's dictionary on what it sampled.
            self.claim(look, unit, record(Form::Raw, 0, &bytes), &bytes, ticket)
        };
        self.end_put();
        match won {
            Ok(()) => Ok(ticket),
            Err((kept, there)) if kept == content => Ok(there),
            Err((kept, _)) => Err(Mismatch { key, kept, given: content }),
        }
    }

    /// zstd refusing a value is a broken build of it (a missing feature), not something a
    /// value can cause: the store stops rather than keep the value another way. The put still
    /// takes its ticket, which is never durable.
    fn fail(&mut self, kind: Kind, why: String) -> Ticket {
        _ = self.segment.failed.set(format!("compressing a {} value: {why}", kind.name()));
        let ticket = Ticket::of(self.segment.number, self.puts + 1);
        self.end_put();
        ticket
    }

    /// Writes a record and publishes its key; on a key published meanwhile, gives back the
    /// content and ticket of what is there and keeps nothing.
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
        let delta = &look.view.tiers[0].delta;
        match delta.insert(look.held, spot) {
            Ok((row, _)) => {
                delta.set_flag(row, Flag::New as usize);
                self.at += span;
                Ok(())
            }
            Err((_, (_, there))) => Err((there.content, there.ticket)),
        }
    }

    /// Writes a record no key will point at, kept for its bytes alone (see [`COPY`]).
    fn write_copy(&mut self, unit: Unit, header: &RecordHeader, payload: &[u8]) {
        let (_, span) = self.write(unit, header, payload, COPY);
        self.at += span;
    }

    /// Writes a record at the writer's next bytes, not yet kept.
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
        // SAFETY: this writer is the segment's, and `at` is past what it published or kept, so
        // no place points there.
        unsafe {
            tail.write(at, &frame);
            tail.write(body, &header.encode());
            tail.write(body + RECORD_HEADER as usize, payload);
            tail.zero(end, at + span as usize - end);
        }
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

    /// Keeps the record [`Writer::write`] wrote last, `span` bytes: the store's own records
    /// (moved by a settle), whose keys it points at itself.
    pub(crate) fn keep(&mut self, span: u64) {
        self.at += span;
        let tail = self.tail();
        tail.publish(self.at, self.puts_here);
    }

    fn tail(&self) -> &Chunk {
        // SAFETY: the tail is never freed while this writer writes it (`Segment::unlink_first`
        // frees only chunks with a next).
        unsafe { self.tail.as_ref() }
    }

    /// Seals the tail with what it holds and moves on to a chunk with room for `need` bytes.
    fn roll(&mut self, need: u64) {
        let old = self.tail();
        let next = Chunk::new((need as usize).max(CHUNK), old.start + old.cap());
        old.publish(self.at, self.puts_here);
        old.next.store(next.as_ptr(), Ordering::Release);
        self.tail = next;
        self.at = 0;
        self.puts_here = 0;
    }

    fn end_put(&mut self) {
        self.puts += 1;
        self.puts_here += 1;
        let tail = self.tail();
        tail.publish(self.at, self.puts_here);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_publishes_bytes_and_puts_in_one_word() {
        let chunk = Chunk::new(4096, 0);
        // SAFETY: made just now; freed below.
        let it = unsafe { chunk.as_ref() };
        assert_eq!(it.published(), (0, 0));
        it.publish(4000, 3);
        assert_eq!(it.published(), (4000, 3));
        // SAFETY: nothing else holds it.
        drop(unsafe { Box::from_raw(chunk.as_ptr()) });
    }
}
