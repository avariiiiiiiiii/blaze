//! The process's chunks: 2 MB blocks of one reserved space, handed to arenas and taken back
//! for the next (docs/v2/plan/design/memory.md "back to the pool, never to the OS in the warm
//! loop").
//!
//! The space is reserved once ([`SPACE_BYTES`] of address space, nothing committed) and carved
//! in order; a chunk given back goes on a lock-free stack and is the next one taken, warm in
//! cache and already backed by pages. [`trim`] lets the kernel take the pages of cached chunks
//! (they stay committed and come back zeroed), for after a burst. Each chunk is aligned to a
//! huge page, and on Linux the space is advised to use them.
//!
//! Cost: a take or give is one compare-and-swap on the stack's head; carving is one
//! `fetch_add`, plus a commit system call every quarter of what is committed.

use std::{ptr::NonNull, sync::OnceLock};

use crate::{
    MemError, Region, Tag,
    stack::{LinkTable, Stack},
    sync::atomic::{AtomicU32, Ordering},
    tag,
};

/// The size of a chunk.
pub const CHUNK: usize = crate::HUGE_PAGE;

/// The address space reserved for chunks: 256 GB, 131,072 chunks. Free on a 64-bit machine
/// (x86-64 and arm64 give user space 128 TB).
#[cfg(target_pointer_width = "64")]
pub const SPACE_BYTES: usize = 256 << 30;

/// The address space reserved for chunks on a 32-bit target: 256 MB, 128 chunks. There
/// (wasm32) a reservation is memory taken whole, so it is a budget, not free address space.
#[cfg(not(target_pointer_width = "64"))]
pub const SPACE_BYTES: usize = 256 << 20;

const COUNT: u32 = (SPACE_BYTES / CHUNK) as u32;

struct Space {
    region: Region,
    links: LinkTable,
    carved: AtomicU32,
    /// Chunks given back, their pages resident.
    warm: Stack,
    /// Chunks [`trim`] purged: committed, their pages given to the kernel.
    cold: Stack,
}

static SPACE: OnceLock<Result<Space, String>> = OnceLock::new();

fn space() -> Result<&'static Space, MemError> {
    let space = SPACE.get_or_init(|| {
        let made = Region::reserve_huge(SPACE_BYTES, Tag::Chunks).and_then(|region| {
            Ok(Space {
                region,
                links: LinkTable::new(COUNT, Tag::Chunks)?,
                carved: AtomicU32::new(0),
                warm: Stack::new(),
                cold: Stack::new(),
            })
        });
        made.map_err(|error| error.to_string()) // alloc: once, when the space cannot be reserved
    });
    match space {
        Ok(space) => Ok(space),
        Err(message) => Err(MemError::Reserve {
            tag: Tag::Chunks,
            bytes: SPACE_BYTES,
            os: std::io::Error::other(message.as_str()),
        }),
    }
}

/// A chunk held by one owner: the arena it was taken for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Chunk(u32);

impl Chunk {
    pub(crate) fn index(&self) -> u32 {
        self.0
    }

    /// The chunk's first byte; the chunk is `CHUNK` bytes, committed, aligned to `CHUNK`.
    pub(crate) fn ptr(&self) -> NonNull<u8> {
        // A chunk exists only once the space does.
        let space = SPACE.get().and_then(|space| space.as_ref().ok());
        match space {
            Some(space) => space.region.at(self.0 as usize * CHUNK),
            None => unreachable!("a chunk outlived its space"),
        }
    }

    /// The chunk `index` names, which the caller holds (it took it and linked it in a list
    /// of its own).
    pub(crate) fn from_index(index: u32) -> Chunk {
        Chunk(index)
    }
}

/// Takes a chunk for `owner`: a warm one if any was given back, else a purged one, else a new
/// one carved from the space.
pub(crate) fn take(owner: Tag) -> Result<Chunk, MemError> {
    let space = space()?;
    let index = match space.warm.pop(&space.links) {
        Some(index) => index,
        None => match space.cold.pop(&space.links) {
            Some(index) => index,
            None => carve(space)?,
        },
    };
    tag::transfer(Tag::Chunks, owner, CHUNK);
    Ok(Chunk(index))
}

#[cold]
fn carve(space: &Space) -> Result<u32, MemError> {
    let index = space.carved.fetch_add(1, Ordering::Relaxed);
    if index >= COUNT {
        space.carved.store(COUNT, Ordering::Relaxed);
        return Err(MemError::Exhausted {
            tag: Tag::Chunks,
            reserved: SPACE_BYTES,
            wanted: (index as usize + 1) * CHUNK,
        });
    }
    space.region.ensure((index as usize + 1) * CHUNK)?;
    // SAFETY: `index` came from the `fetch_add` just now, so no other thread has it.
    unsafe { space.links.init(index) }?;
    Ok(index)
}

/// Gives back a chunk `owner` held; its contents are dead.
pub(crate) fn give(chunk: Chunk, owner: Tag) {
    let Some(Ok(space)) = SPACE.get() else { unreachable!("a chunk outlived its space") };
    tag::transfer(owner, Tag::Chunks, CHUNK);
    space.warm.push(chunk.0, &space.links);
}

/// The link of a held chunk, for its owner's list of the chunks it holds (a chunk is on one
/// list at a time: an owner's or the cache's).
pub(crate) fn link(chunk: u32) -> &'static AtomicU32 {
    let Some(Ok(space)) = SPACE.get() else { unreachable!("a chunk outlived its space") };
    crate::stack::Links::link(&space.links, chunk)
}

/// Lets the kernel take the pages of every warm cached chunk; they stay committed and come
/// back zeroed. Returns how many it purged. For after a burst, never in the warm loop.
pub fn trim() -> usize {
    let Some(Ok(space)) = SPACE.get() else { return 0 };
    let mut purged = 0;
    while let Some(index) = space.warm.pop(&space.links) {
        // SAFETY: the chunk was popped off the cache, so no owner holds it and no one reads
        // its contents before writing them.
        unsafe { space.region.purge(index as usize * CHUNK, CHUNK) };
        space.cold.push(index, &space.links);
        purged += 1;
    }
    purged
}

/// Bytes of chunks carved so far (held or cached).
pub fn carved_bytes() -> usize {
    let Some(Ok(space)) = SPACE.get() else { return 0 };
    space.carved.load(Ordering::Relaxed).min(COUNT) as usize * CHUNK
}
