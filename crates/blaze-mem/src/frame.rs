//! The frame allocator: a bump pointer over a reserved region, one per worker per nesting
//! level, reset when the task that used it ends (docs/v2/plan/design/memory.md, "a task").
//!
//! [`task`] gives the closure a frame of the calling thread's, empty, and resets it when the
//! closure returns (or unwinds); a task inside a task gets a frame of its own, so an inner
//! reset never frees what the outer task holds. Within a task, [`Frame::scope`] and
//! [`Marker`]s roll back to a point. A frame implements [`Allocator`], so `Vec<T, &Frame>`,
//! `Box<T, &Frame>` and the rest allocate from it.
//!
//! Resetting is one store: nothing is freed piecemeal, no destructor of a value placed with
//! [`Frame::alloc`] runs (collections in a frame drop their elements as usual). The pages a
//! frame committed stay committed, so the next task on the worker allocates from warm memory
//! with no system call; [`Frame::trim`] gives them back after a burst.
//!
//! Cost: an allocation is an align, an add, a compare against the committed end and two
//! stores to the frame (no atomic, no shared line); about 2 ns. Freeing or growing the most
//! recent allocation happens in place, so a `Vec<T, &Frame>` pushed in a loop never copies
//! while nothing else is allocated after it.

use std::{
    alloc::{AllocError, Allocator, Layout},
    cell::{Cell, RefCell},
    mem::{self, MaybeUninit},
    ptr::{self, NonNull},
    slice, str,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::{MemError, Region, Tag, error::fail, tag};

/// The address space one frame reserves: what one task may take at most.
pub const FRAME_RESERVE: usize = 4 << 30;

/// Frames a thread keeps between tasks: the nesting depth whose frames are kept warm.
const DEPTH: usize = 8;

static IDS: AtomicU32 = AtomicU32::new(0);

/// A bump allocator over a reserved region. Not `Sync`: a frame is its worker's.
pub struct Frame {
    region: Region,
    /// Offset of the next free byte.
    cursor: Cell<usize>,
    /// The committed end as last seen, so the common allocation reads no atomic.
    limit: Cell<usize>,
    /// The most `cursor` was since the last reset.
    peak: Cell<usize>,
    /// The subsystem of the task using it, for [`tag::task_ended`].
    tag: Cell<Tag>,
    id: u32,
}

/// A point in a frame to roll back to with [`Frame::reset_to`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Marker {
    frame: u32,
    offset: usize,
}

impl Frame {
    /// A frame for tasks of `tag`, reserving [`FRAME_RESERVE`] on huge pages.
    pub fn new(tag: Tag) -> Result<Frame, MemError> {
        Frame::with_reserve(FRAME_RESERVE, tag)
    }

    pub fn with_reserve(bytes: usize, tag: Tag) -> Result<Frame, MemError> {
        let region = Region::reserve_huge(bytes, Tag::Frames)?;
        Ok(Frame {
            region,
            cursor: Cell::new(0),
            limit: Cell::new(0),
            peak: Cell::new(0),
            tag: Cell::new(tag),
            id: IDS.fetch_add(1, Ordering::Relaxed),
        })
    }

    pub fn tag(&self) -> Tag {
        self.tag.get()
    }

    /// Bytes in use since the last reset (alignment padding included).
    pub fn used(&self) -> usize {
        self.cursor.get()
    }

    /// The most bytes in use at once since the last reset.
    pub fn peak(&self) -> usize {
        self.peak.get()
    }

    /// Bytes committed: the high-water mark of every task it served, until trimmed.
    pub fn committed(&self) -> usize {
        self.region.committed()
    }

    pub fn reserved(&self) -> usize {
        self.region.reserved()
    }

    /// Whether `ptr` points into this frame's reservation.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.region.base().as_ptr() as usize;
        (base..base + self.region.reserved()).contains(&(ptr as usize))
    }

    fn offset_of(&self, ptr: NonNull<u8>) -> usize {
        ptr.as_ptr() as usize - self.region.base().as_ptr() as usize
    }

    /// Takes `layout`'s bytes, aligned; `MemError` when the reservation is used up or the
    /// kernel refuses to commit.
    #[inline]
    pub fn try_alloc_layout(&self, layout: Layout) -> Result<NonNull<u8>, MemError> {
        let base = self.region.base().as_ptr() as usize;
        let at = base + self.cursor.get();
        let aligned = (at + layout.align() - 1) & !(layout.align() - 1);
        let offset = aligned - base;
        let end = offset + layout.size();
        if end > self.limit.get() {
            self.commit(end)?;
        }
        self.cursor.set(end);
        if end > self.peak.get() {
            self.peak.set(end);
        }
        Ok(self.region.at(offset))
    }

    #[cold]
    fn commit(&self, end: usize) -> Result<(), MemError> {
        let result = self.region.ensure(end).map_err(|error| match error {
            MemError::Exhausted { reserved, wanted, .. } => {
                MemError::Exhausted { tag: self.tag.get(), reserved, wanted }
            }
            other => other,
        });
        self.limit.set(self.region.committed());
        result
    }

    /// Takes `layout`'s bytes, aligned; stops with the error when memory cannot be had.
    #[inline]
    #[track_caller]
    pub fn alloc_layout(&self, layout: Layout) -> NonNull<u8> {
        match self.try_alloc_layout(layout) {
            Ok(ptr) => ptr,
            Err(error) => fail(error),
        }
    }

    /// Places `value` in the frame. Its destructor never runs: the frame forgets it at reset.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc<T>(&self, value: T) -> &mut T {
        let ptr = self.alloc_layout(Layout::new::<T>()).cast::<T>();
        // SAFETY: `ptr` is fresh memory of this frame, sized and aligned for `T`, handed out
        // once; it stays valid until a reset, which takes `&mut self` and so outlives this
        // borrow.
        unsafe { ptr.write(value) };
        // SAFETY: just initialized, and no other reference to it exists.
        unsafe { &mut *ptr.as_ptr() }
    }

    /// Copies `values` into the frame.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc_slice_copy<T: Copy>(&self, values: &[T]) -> &mut [T] {
        let slots = self.alloc_uninit_slice::<T>(values.len());
        for (slot, value) in slots.iter_mut().zip(values) {
            slot.write(*value);
        }
        // SAFETY: every element was just written.
        unsafe { slots.assume_init_mut() }
    }

    /// `len` values made by `f(0)`, `f(1)`, ... in the frame. If `f` panics, the values made
    /// so far are forgotten with the frame, as every frame value is.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc_slice_fill_with<T>(&self, len: usize, mut f: impl FnMut(usize) -> T) -> &mut [T] {
        let slots = self.alloc_uninit_slice::<T>(len);
        for (i, slot) in slots.iter_mut().enumerate() {
            slot.write(f(i));
        }
        // SAFETY: every element was just written.
        unsafe { slots.assume_init_mut() }
    }

    /// The items of `iter`, which says its length, in the frame.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc_slice_from_iter<T, I>(&self, iter: I) -> &mut [T]
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
    {
        let mut iter = iter.into_iter();
        let len = iter.len();
        let slots = self.alloc_uninit_slice::<T>(len);
        let mut filled = 0;
        for slot in slots.iter_mut() {
            match iter.next() {
                Some(value) => {
                    slot.write(value);
                    filled += 1;
                }
                None => break,
            }
        }
        // SAFETY: the first `filled` elements were written; an iterator that said more than it
        // gave leaves the rest out of the slice.
        unsafe { slots[..filled].assume_init_mut() }
    }

    /// Room for `len` values of `T`, uninitialized.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc_uninit_slice<T>(&self, len: usize) -> &mut [MaybeUninit<T>] {
        let layout = match Layout::array::<T>(len) {
            Ok(layout) => layout,
            Err(_) => fail(MemError::TooLarge { tag: self.tag(), what: "a frame slice", wanted: len }),
        };
        let ptr = self.alloc_layout(layout).cast::<MaybeUninit<T>>();
        // SAFETY: fresh memory of this frame for `len` elements, aligned, handed out once and
        // valid until a reset (which takes `&mut self`); `MaybeUninit` needs no initialization.
        unsafe { slice::from_raw_parts_mut(ptr.as_ptr(), len) }
    }

    /// Copies `text` into the frame.
    #[inline]
    #[track_caller]
    #[allow(clippy::mut_from_ref, reason = "each call hands out fresh, disjoint memory")]
    pub fn alloc_str(&self, text: &str) -> &mut str {
        let bytes = self.alloc_slice_copy(text.as_bytes());
        // SAFETY: the bytes are a copy of a `str`'s, so UTF-8.
        unsafe { str::from_utf8_unchecked_mut(bytes) }
    }

    /// The point to roll back to.
    pub fn mark(&self) -> Marker {
        Marker { frame: self.id, offset: self.cursor.get() }
    }

    /// Frees everything allocated since `marker` was taken. Stops when the marker is another
    /// frame's or from after a rollback past it.
    #[track_caller]
    pub fn reset_to(&mut self, marker: Marker) {
        assert_eq!(marker.frame, self.id, "a marker of another frame");
        assert!(
            marker.offset <= self.cursor.get(),
            "a marker taken after a point the frame was already rolled back past"
        );
        self.cursor.set(marker.offset);
    }

    /// Frees everything. Counts the task's peak to its tag ([`tag::task_ended`]).
    pub fn reset(&mut self) {
        tag::task_ended(self.tag.get(), self.peak.get());
        self.cursor.set(0);
        self.peak.set(0);
    }

    /// Runs `f` with this frame and frees what it allocated once it returns. What `f` returns
    /// cannot borrow the frame, so nothing it allocated outlives the rollback.
    pub fn scope<R>(&mut self, f: impl FnOnce(&Frame) -> R) -> R {
        struct Back<'f> {
            frame: &'f Frame,
            offset: usize,
        }
        impl Drop for Back<'_> {
            fn drop(&mut self) {
                self.frame.cursor.set(self.offset);
            }
        }
        let back = Back { frame: self, offset: self.cursor.get() };
        f(back.frame)
    }

    /// Gives back to the kernel the pages past `keep` bytes (or past what is in use, if more).
    pub fn trim(&mut self, keep: usize) {
        let keep = keep.max(self.cursor.get());
        // A refused decommit leaves the pages committed: memory held, nothing broken.
        let _ = self.region.decommit_after(keep);
        self.limit.set(self.region.committed());
    }

    pub(crate) fn set_tag(&self, tag: Tag) {
        self.tag.set(tag);
    }

    /// Whether `[ptr, ptr + size)` ends at the cursor: the most recent allocation.
    fn is_top(&self, ptr: NonNull<u8>, size: usize) -> bool {
        self.offset_of(ptr) + size == self.cursor.get()
    }
}

// SAFETY: blocks are disjoint ranges of the frame's reservation, handed out once each and
// committed before they are returned. A block stays valid until it is deallocated or the frame
// is reset, rolled back or dropped, each of which takes `&mut Frame` (or ownership), which no
// live `&Frame` an allocation borrows allows. `deallocate` and `shrink` only move the cursor
// back over the top block, never touching another; nothing here panics or unwinds.
unsafe impl Allocator for Frame {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let ptr = self.try_alloc_layout(layout).map_err(|_| AllocError)?;
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if self.is_top(ptr, layout.size()) {
            self.cursor.set(self.offset_of(ptr));
        }
    }

    #[inline]
    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if self.is_top(ptr, old.size()) && ptr.as_ptr().align_offset(new.align()) == 0 {
            let end = self.offset_of(ptr) + new.size();
            if end > self.limit.get() {
                self.commit(end).map_err(|_| AllocError)?;
            }
            self.cursor.set(end);
            if end > self.peak.get() {
                self.peak.set(end);
            }
            return Ok(NonNull::slice_from_raw_parts(ptr, new.size()));
        }
        let to = self.try_alloc_layout(new).map_err(|_| AllocError)?;
        // SAFETY: the caller says `ptr` is a live block of `old.size()` bytes; `to` is a fresh
        // block of at least as many, so the two do not overlap.
        unsafe { ptr::copy_nonoverlapping(ptr.as_ptr(), to.as_ptr(), old.size()) };
        Ok(NonNull::slice_from_raw_parts(to, new.size()))
    }

    #[inline]
    unsafe fn grow_zeroed(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's guarantees for `grow_zeroed` are those of `grow`.
        let block = unsafe { self.grow(ptr, old, new) }?;
        let tail = block.cast::<u8>().as_ptr().wrapping_add(old.size());
        // SAFETY: the block holds `new.size()` bytes, so `[old.size(), new.size())` is inside
        // it; memory reused after a reset may hold old bytes, hence the explicit zeroing.
        unsafe { ptr::write_bytes(tail, 0, new.size() - old.size()) };
        Ok(block)
    }

    #[inline]
    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if ptr.as_ptr().align_offset(new.align()) != 0 {
            let to = self.try_alloc_layout(new).map_err(|_| AllocError)?;
            // SAFETY: `ptr` is a live block of at least `new.size()` bytes and `to` a fresh one.
            unsafe { ptr::copy_nonoverlapping(ptr.as_ptr(), to.as_ptr(), new.size()) };
            return Ok(NonNull::slice_from_raw_parts(to, new.size()));
        }
        if self.is_top(ptr, old.size()) {
            self.cursor.set(self.offset_of(ptr) + new.size());
        }
        Ok(NonNull::slice_from_raw_parts(ptr, new.size()))
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("tag", &self.tag.get())
            .field("used", &self.cursor.get())
            .field("peak", &self.peak.get())
            .field("committed", &self.committed())
            .finish()
    }
}

struct Spare {
    frames: [Option<Frame>; DEPTH],
    len: usize,
}

thread_local! {
    /// The calling thread's frames between tasks, warm: the next task takes the top one.
    static SPARE: RefCell<Spare> = const {
        RefCell::new(Spare { frames: [const { None }; DEPTH], len: 0 })
    };
}

fn take_spare() -> Option<Frame> {
    SPARE
        .try_with(|spare| {
            let mut spare = spare.borrow_mut();
            if spare.len == 0 {
                return None;
            }
            spare.len -= 1;
            let len = spare.len;
            spare.frames[len].take()
        })
        .ok()
        .flatten()
}

fn give_spare(frame: Frame) {
    let kept = SPARE.try_with(|spare| {
        let mut spare = spare.borrow_mut();
        if spare.len == DEPTH {
            return Some(frame);
        }
        let len = spare.len;
        spare.frames[len] = Some(frame);
        spare.len += 1;
        None
    });
    // A thread nested deeper than `DEPTH` or ending: the frame is unmapped.
    match kept {
        Ok(Some(frame)) => drop(frame),
        Ok(None) => {}
        Err(_) => {}
    }
}

/// Runs a task of `tag` with a frame of the calling thread's, empty, and resets it when `f`
/// returns or unwinds. A task inside a task gets another frame, so the inner reset frees only
/// the inner task's memory. What `f` returns cannot borrow the frame.
///
/// The first task at each nesting depth of a thread reserves its frame (a system call, no
/// global allocation); after that a task costs two thread-local accesses.
#[track_caller]
pub fn task<R>(tag: Tag, f: impl FnOnce(&Frame) -> R) -> R {
    struct Back(Option<Frame>);
    impl Drop for Back {
        fn drop(&mut self) {
            if let Some(mut frame) = self.0.take() {
                frame.reset();
                give_spare(frame);
            }
        }
    }
    let frame = match take_spare() {
        Some(frame) => frame,
        None => match Frame::new(tag) {
            Ok(frame) => frame,
            Err(error) => fail(error),
        },
    };
    frame.set_tag(tag);
    let back = Back(Some(frame));
    match &back.0 {
        Some(frame) => f(frame),
        None => unreachable!("the frame is taken back only on drop"),
    }
}

/// Frames the calling thread keeps warm between tasks.
pub fn spare_frames() -> usize {
    SPARE.try_with(|spare| spare.borrow().len).unwrap_or(0)
}

/// Trims every frame the calling thread keeps to `keep` bytes committed: what a worker does
/// when it goes idle after a burst.
pub fn trim_spare(keep: usize) {
    let _ = SPARE.try_with(|spare| {
        let mut spare = spare.borrow_mut();
        for frame in spare.frames.iter_mut().flatten() {
            frame.trim(keep);
        }
    });
}

const _: () = assert!(mem::size_of::<Marker>() == 16);
