//! Scratch buffers reused per worker (docs/v2/plan/design/memory.md, "Scratch buffers reused
//! per worker"): formatting, hashing, encoding and decompression write into a buffer the
//! worker keeps, grown once to its high-water mark and never freed in the warm loop.
//!
//! [`with_bytes`], [`with_u32s`], [`with_u64s`] and [`with_string`] lend the calling thread's
//! buffer, empty, for the closure; a call inside a call gets another of the thread's (up to
//! [`DEPTH`] deep, each kept warm). [`Scratch<T>`] is the same for a type of the caller's, kept
//! in its worker state.
//!
//! Cost: two thread-local accesses and a `clear`; an allocation only while a buffer grows past
//! the most it ever held.

use std::cell::RefCell;

/// Buffers kept per thread and kind: the nesting depth whose buffers stay warm.
pub const DEPTH: usize = 4;

struct Shelf<B> {
    buffers: [B; DEPTH],
    /// Buffers on the shelf; `buffers[..free]` are there, the rest are lent out.
    free: usize,
}

impl<B: Default> Shelf<B> {
    fn take(&mut self) -> Option<B> {
        if self.free == 0 {
            return None;
        }
        self.free -= 1;
        Some(std::mem::take(&mut self.buffers[self.free]))
    }

    fn give(&mut self, buffer: B) {
        if self.free < DEPTH {
            self.buffers[self.free] = buffer;
            self.free += 1;
        }
    }
}

macro_rules! shelves {
    ($($(#[$doc:meta])* $name:ident, $shelf:ident: $buffer:ty;)*) => {
        thread_local! {
            $(
                static $shelf: RefCell<Shelf<$buffer>> = const {
                    RefCell::new(Shelf { buffers: [const { <$buffer>::new() }; DEPTH], free: DEPTH })
                };
            )*
        }

        $(
            $(#[$doc])*
            pub fn $name<R>(f: impl FnOnce(&mut $buffer) -> R) -> R {
                let mut buffer = $shelf
                    .try_with(|shelf| shelf.borrow_mut().take())
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                buffer.clear();
                let result = f(&mut buffer);
                buffer.clear();
                let back = $shelf.try_with(|shelf| shelf.borrow_mut().give(buffer));
                // A thread that is ending, or nested past `DEPTH`: the buffer is freed.
                drop(back);
                result
            }
        )*
    };
}

shelves! {
    /// Lends the thread's byte buffer, empty.
    with_bytes, BYTES: Vec<u8>;
    /// Lends the thread's `u32` buffer, empty.
    with_u32s, U32S: Vec<u32>;
    /// Lends the thread's `u64` buffer, empty.
    with_u64s, U64S: Vec<u64>;
    /// Lends the thread's string buffer, empty: formatting at the edge.
    with_string, STRINGS: String;
}

/// A buffer of `T`s a worker keeps across calls: [`Scratch::take`] lends it empty, keeping
/// what it grew to.
#[derive(Default)]
pub struct Scratch<T> {
    buffer: Vec<T>,
}

impl<T> Scratch<T> {
    pub const fn new() -> Scratch<T> {
        Scratch { buffer: Vec::new() }
    }

    /// The buffer, emptied.
    pub fn take(&mut self) -> &mut Vec<T> {
        self.buffer.clear();
        &mut self.buffer
    }

    /// The most elements it has room for without growing.
    pub fn high_water(&self) -> usize {
        self.buffer.capacity()
    }

    /// Frees the buffer: after a burst, so it grows again only to the next working set.
    pub fn shrink(&mut self, keep: usize) {
        self.buffer.clear();
        self.buffer.shrink_to(keep);
    }
}
