//! The memory one compiled expansion runs in (docs/v2/plan/lanes/fastcompile/macros-tier.md,
//! "Memory"): one reservation of [`SPAN`] bytes aligned to [`SPAN`], and [`TAIL_GUARD`]
//! inaccessible bytes after it. A guest address is an offset into it, and every load and
//! store of the confined code is at `base + (address + offset) mod 2^32`: no address the code
//! computes reaches outside the reservation, whatever it is. The parts the macro may use are
//! mapped; the rest faults:
//!
//! ```text
//! [0, G)                  null guard: null and small offsets from it fault
//! [G, 2G)                 header: the cells the confined code reads (`cell`)
//! [2G, ..)                image: writable statics and thread-locals, then read-only data
//! G                       guard below the shadow stack
//! stack                   the shadow stack: address-taken locals, growing down
//! heap                    the macro's heap, committed as it grows, up to its bound
//! .. 2^32                 never mapped
//! [2^32, 2^32 + G)        tail guard: the widest access at the last address faults here
//! ```
//!
//! `G` is [`GRANULE`], a multiple of every page size blaze runs on, so every part has its
//! own protection. The machine's stack (spill slots, saved registers, return addresses) is
//! never in the region: no guest address names it.
//!
//! This is the crate's one unsafe module. Its safe API checks every range against the parts
//! that are mapped before touching a byte, so a shim handed any address by the macro reads
//! and writes only mapped memory of this region.

#![allow(unsafe_code)]

use std::{ops::Range, ptr::NonNull};

use crate::guest::Stop;

const _: () = assert!(usize::BITS == 64, "the compiled tier needs a 64-bit address space");

/// Bytes a guest address reaches: the low 32 bits of an address are all a confined access
/// keeps.
pub(crate) const SPAN: u64 = 1 << 32;
/// The unit of the layout: 64 KiB, a multiple of 4 KiB (x86-64, most arm64 Linux), 16 KiB
/// (Apple silicon) and 64 KiB pages, and Windows' allocation granularity.
pub(crate) const GRANULE: u32 = 64 << 10;
/// Inaccessible bytes past the span: an access of up to 16 bytes (the widest a confined
/// load or store makes) at the span's last address faults here, inside the reservation.
pub(crate) const TAIL_GUARD: usize = GRANULE as usize;

/// The header's cells, by offset from [`Layout::HEADER`]: what the confined code and the
/// shims share. Every one is guest-writable memory, so no check of the sandbox reads one: a
/// macro that overwrites them breaks only itself.
pub(crate) mod cell {
    /// The shadow stack's pointer (`u64`, a guest address): a function with stack slots
    /// takes its frame below it and puts it back on return and at each landing pad.
    pub(crate) const SHADOW_SP: u32 = 0;
    /// The lowest address the shadow stack may reach (`u64`): a frame below it traps.
    pub(crate) const SHADOW_LIMIT: u32 = 8;
    /// The running thread's `errno` (`i32`): `__errno_location`/`__error` give its address.
    pub(crate) const ERRNO: u32 = 16;
    /// Eight zero bytes, never written by blaze: the address of data externs that only need
    /// to exist (`__rust_no_alloc_shim_is_unstable`).
    pub(crate) const ZERO: u32 = 24;
    /// Bytes of cells, rounded to 16.
    pub(crate) const END: u32 = 32;
}

/// The sizes the macro's memory is bounded by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Bounds {
    /// Bytes of shadow stack.
    pub(crate) stack: u32,
    /// Bytes of heap.
    pub(crate) heap: u32,
}

impl Bounds {
    /// 8 MiB of shadow stack, as a Rust main thread has of stack, and 2 GiB of heap.
    pub(crate) const DEFAULT: Bounds = Bounds { stack: 8 << 20, heap: 2 << 30 };
}

/// Where each part of the region is: offsets, which are guest addresses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Layout {
    /// The writable part of the image: statics and thread-locals.
    pub(crate) writable: Range<u32>,
    /// The read-only part: constants, string literals, vtables.
    pub(crate) readonly: Range<u32>,
    /// The shadow stack, growing down from its end.
    pub(crate) stack: Range<u32>,
    /// The heap's whole bound; only a prefix of it is committed at a time.
    pub(crate) heap: Range<u32>,
}

impl Layout {
    pub(crate) const HEADER: u32 = GRANULE;
    const IMAGE: u32 = 2 * GRANULE;

    /// The layout of an image with `writable` and `readonly` bytes, in `bounds`. An error when
    /// it doesn't fit in [`SPAN`].
    pub(crate) fn new(writable: u32, readonly: u32, bounds: Bounds) -> Result<Layout, Stop> {
        let up = |at: u64| at.next_multiple_of(u64::from(GRANULE));
        let writable_end = u64::from(Layout::IMAGE) + up(u64::from(writable));
        let readonly_end = writable_end + up(u64::from(readonly));
        let stack_start = readonly_end + u64::from(GRANULE);
        let stack_end = stack_start + up(u64::from(bounds.stack));
        let heap_end = stack_end + up(u64::from(bounds.heap));
        if heap_end > SPAN {
            // alloc: error path
            return Err(Stop(format!(
                "needs {writable} bytes of statics, {readonly} of constants, {} of stack and {} of \
                 heap, more than the compiled tier's 4 GiB",
                bounds.stack, bounds.heap
            )));
        }
        let at = |it: u64| it as u32;
        Ok(Layout {
            writable: Layout::IMAGE..at(Layout::IMAGE as u64 + u64::from(writable)),
            readonly: at(writable_end)..at(writable_end + u64::from(readonly)),
            stack: at(stack_start)..at(stack_end),
            heap: at(stack_end)..at(heap_end),
        })
    }

    /// The image's whole extent, from its start to the end of its read-only part's granule.
    fn image(&self) -> Range<u32> {
        Layout::IMAGE..self.readonly.end.next_multiple_of(GRANULE)
    }

    fn readonly_pages(&self) -> Range<u32> {
        self.readonly.start..self.readonly.end.next_multiple_of(GRANULE)
    }
}

/// One reservation and what of it is mapped. Compiled code writes its memory only while the
/// runner holds `&mut Region` (`tier.rs`, "Running"), so a `&[u8]` [`Region::read`] gives
/// is never written under it.
pub(crate) struct Region {
    base: NonNull<u8>,
    layout: Layout,
    /// Bytes of the heap mapped, from its start: a multiple of [`GRANULE`].
    committed: u32,
    /// Whether [`Region::install`] made the read-only part read-only.
    installed: bool,
}

// SAFETY: a region is an address range only this value reaches (nothing else knows `base`
// but the code running in it, which runs only under `&mut Region`); moving it to another
// thread moves that exclusive use with it. It is not `Sync`: shared reads while another
// thread runs the macro would race its writes.
unsafe impl Send for Region {}

impl Region {
    /// Reserves a region for `layout` and maps its header, image and stack, zeroed. The
    /// image is filled by [`Region::install`].
    pub(crate) fn new(layout: Layout) -> Result<Region, Stop> {
        let base = os::reserve().map_err(|why| {
            // alloc: error path
            Stop(format!("could not reserve the compiled tier's 4 GiB of address space: {why}"))
        })?;
        let region = Region { base, layout, committed: 0, installed: false };
        let header = Layout::HEADER..Layout::HEADER + GRANULE;
        let stack = region.layout.stack.start..region.layout.stack.end;
        for part in [header, region.layout.image(), stack] {
            region.map(part)?;
        }
        Ok(region)
    }

    /// The reservation's address, aligned to [`SPAN`]: what the confined code adds to every
    /// guest address (its `vmctx`).
    pub(crate) fn base(&self) -> u64 {
        self.base.as_ptr() as u64
    }

    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Bytes of the heap mapped now.
    pub(crate) fn committed(&self) -> u32 {
        self.committed
    }

    /// Writes the image (`writable` then `readonly`, each its part's length), makes the
    /// read-only part read-only, and sets the header's cells.
    pub(crate) fn install(&mut self, writable: &[u8], readonly: &[u8]) -> Result<(), Stop> {
        if self.installed {
            // alloc: error path
            return Err(Stop("was given a second image for one region".to_owned()));
        }
        self.copy_in(Part::Writable, writable)?;
        self.copy_in(Part::Readonly, readonly)?;
        let pages = self.layout.readonly_pages();
        if !pages.is_empty() {
            // SAFETY: `pages` is inside the reservation (`Layout::new` keeps every part below
            // `SPAN`), granule-aligned, and mapped by `new`; only its protection changes.
            unsafe { os::protect_readonly(self.at(pages.start), pages.len()) }.map_err(|why| {
                // alloc: error path
                Stop(format!("could not make the compiled tier's constants read-only: {why}"))
            })?;
        }
        self.installed = true;
        self.reset_cells();
        Ok(())
    }

    /// Back to the state after [`Region::install`] for the next expansion: the writable
    /// image as `writable` gives it and fresh cells. The stack and heap keep their bytes:
    /// the heap hands out none of them before they are written or zeroed.
    pub(crate) fn reset(&mut self, writable: &[u8]) -> Result<(), Stop> {
        self.copy_in(Part::Writable, writable)?;
        self.reset_cells();
        Ok(())
    }

    fn reset_cells(&mut self) {
        let mut cells = [0u8; cell::END as usize];
        let sp = u64::from(self.layout.stack.end);
        let limit = u64::from(self.layout.stack.start);
        cells[cell::SHADOW_SP as usize..][..8].copy_from_slice(&sp.to_le_bytes());
        cells[cell::SHADOW_LIMIT as usize..][..8].copy_from_slice(&limit.to_le_bytes());
        self.raw_write(Layout::HEADER, &cells);
    }

    fn copy_in(&mut self, part: Part, bytes: &[u8]) -> Result<(), Stop> {
        let (range, what) = match part {
            Part::Writable => (&self.layout.writable, "statics"),
            Part::Readonly => (&self.layout.readonly, "constants"),
        };
        let (start, len) = (range.start, range.len());
        if bytes.len() != len {
            // alloc: error path
            return Err(Stop(format!("was given {} bytes of {what} for a layout of {len}", bytes.len())));
        }
        self.raw_write(start, bytes);
        Ok(())
    }

    /// Maps the heap up to at least `len` bytes from its start, in granules.
    pub(crate) fn grow_heap(&mut self, len: u32) -> Result<(), Stop> {
        if len <= self.committed {
            return Ok(());
        }
        let bound = self.layout.heap.len() as u32;
        if len > bound {
            // alloc: error path
            return Err(Stop(format!("used more than its {bound} bytes of heap")));
        }
        // A quarter more than asked, as blaze-mem's regions grow, so a growing heap maps in
        // O(log n) calls.
        let more = u64::from(len) + u64::from(len / 4);
        let want = more.next_multiple_of(u64::from(GRANULE)).min(u64::from(bound)) as u32;
        let start = self.layout.heap.start;
        self.map(start + self.committed..start + want)?;
        self.committed = want;
        Ok(())
    }

    /// Gives the heap's pages past its first `keep` bytes back to the kernel, after an
    /// expansion that grew past what the next ones are likely to need.
    pub(crate) fn shrink_heap(&mut self, keep: u32) -> Result<(), Stop> {
        let keep = keep.next_multiple_of(GRANULE);
        if keep >= self.committed {
            return Ok(());
        }
        let start = self.layout.heap.start;
        // SAFETY: `[start + keep, start + committed)` is mapped heap inside the reservation,
        // granule-aligned; `&mut self` says no code runs in it and nothing borrows it, and
        // `committed` drops below it at once, so no check lets a read reach it again.
        unsafe { os::decommit(self.at(start + keep), (self.committed - keep) as usize) }.map_err(|why| {
            // alloc: error path
            Stop(format!("could not give back the compiled tier's heap: {why}"))
        })?;
        self.committed = keep;
        Ok(())
    }

    fn map(&self, part: Range<u32>) -> Result<(), Stop> {
        if part.is_empty() {
            return Ok(());
        }
        // SAFETY: every caller passes a granule-aligned part of the layout, which `Layout::new`
        // keeps below `SPAN`, so the range is inside the live reservation; mapping it touches
        // no other memory.
        unsafe { os::commit(self.at(part.start), part.len()) }.map_err(|why| {
            // alloc: error path
            Stop(format!("could not map {} bytes of the compiled tier's memory: {why}", part.len()))
        })
    }

    /// The `len` bytes at guest address `at`, when all of them are in one readable part.
    pub(crate) fn read(&self, at: u64, len: usize) -> Result<&[u8], Stop> {
        if len == 0 {
            return Ok(&[]);
        }
        let Some(at) = self.inside(at, len, false) else {
            return Err(outside("read", at, len));
        };
        // SAFETY: `inside` found `[at, at + len)` in one mapped, readable part of the live
        // reservation; compiled code writes the region only under `&mut Region`, so nothing
        // writes these bytes while `&self` lends them.
        Ok(unsafe { std::slice::from_raw_parts(self.at(at).as_ptr(), len) })
    }

    /// Writes `bytes` at guest address `at`, when all of them are in one writable part.
    pub(crate) fn write(&mut self, at: u64, bytes: &[u8]) -> Result<(), Stop> {
        if bytes.is_empty() {
            return Ok(());
        }
        let Some(at) = self.inside(at, bytes.len(), true) else {
            return Err(outside("wrote", at, bytes.len()));
        };
        self.raw_write(at, bytes);
        Ok(())
    }

    /// `len` copies of `byte` at `at`: `calloc`'s zeroing, `memset`.
    pub(crate) fn fill(&mut self, at: u64, len: usize, byte: u8) -> Result<(), Stop> {
        if len == 0 {
            return Ok(());
        }
        let Some(at) = self.inside(at, len, true) else {
            return Err(outside("wrote", at, len));
        };
        // SAFETY: `inside` found `[at, at + len)` in one mapped, writable part of the
        // reservation, and `&mut self` says nothing else reads or writes it now.
        unsafe { self.at(at).as_ptr().write_bytes(byte, len) };
        Ok(())
    }

    /// `memmove` of `len` bytes from `from` to `to`, each range in one part.
    pub(crate) fn copy_within(&mut self, from: u64, to: u64, len: usize) -> Result<(), Stop> {
        if len == 0 {
            return Ok(());
        }
        let Some(src) = self.inside(from, len, false) else {
            return Err(outside("read", from, len));
        };
        let Some(dst) = self.inside(to, len, true) else {
            return Err(outside("wrote", to, len));
        };
        // SAFETY: both ranges are mapped parts of the reservation (`inside`), the destination
        // writable; `copy` allows them to overlap, and `&mut self` says nothing else touches
        // them now.
        unsafe { std::ptr::copy(self.at(src).as_ptr(), self.at(dst).as_ptr(), len) };
        Ok(())
    }

    /// `[at, at + len)` as an offset, when it is all in one part mapped for reading (and for
    /// writing, with `write`).
    fn inside(&self, at: u64, len: usize, write: bool) -> Option<u32> {
        let end = at.checked_add(len as u64)?;
        let header = u64::from(Layout::HEADER)..u64::from(Layout::HEADER + GRANULE);
        let heap = u64::from(self.layout.heap.start)
            ..u64::from(self.layout.heap.start) + u64::from(self.committed);
        let wide = |it: &Range<u32>| u64::from(it.start)..u64::from(it.end);
        let parts = [header, wide(&self.layout.writable), wide(&self.layout.stack), heap];
        let readonly = (!write).then(|| wide(&self.layout.readonly));
        let within = |part: &Range<u64>| part.start <= at && end <= part.end;
        (parts.iter().any(within) || readonly.as_ref().is_some_and(within)).then_some(at as u32)
    }

    /// Writes `bytes` at offset `at`, which the caller has checked is writable.
    fn raw_write(&mut self, at: u32, bytes: &[u8]) {
        debug_assert!(u64::from(at) + bytes.len() as u64 <= SPAN);
        // SAFETY: every caller checked `[at, at + len)` is in one mapped, writable part (the
        // header, the image before `install` protects it, or a part `inside` found), and
        // `&mut self` says nothing else reads or writes it; `bytes` is a host slice, never
        // inside the region (the region lends none mutably).
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.at(at).as_ptr(), bytes.len()) };
    }

    fn at(&self, offset: u32) -> NonNull<u8> {
        // SAFETY: `base` is not null and `offset < SPAN`, so the sum stays inside the
        // reservation, which does not wrap around the address space.
        unsafe { NonNull::new_unchecked(self.base.as_ptr().add(offset as usize)) }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: `base` is the reservation `os::reserve` made, whole, and the region is
        // gone: nothing reaches its memory again (code runs in it only under `&mut Region`).
        unsafe { os::release(self.base) };
    }
}

impl std::fmt::Debug for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Region")
            .field("base", &format_args!("{:#x}", self.base()))
            .field("layout", &self.layout)
            .field("committed", &self.committed)
            .finish()
    }
}

/// A part of the image.
#[derive(Clone, Copy)]
enum Part {
    Writable,
    Readonly,
}

fn outside(did: &str, at: u64, len: usize) -> Stop {
    // alloc: error path
    Stop(format!("{did} {len} bytes at {at:#x}, outside the memory it was given"))
}

/// The reservation's whole size.
const RESERVED: usize = SPAN as usize + TAIL_GUARD;

#[cfg(unix)]
mod os {
    use std::{io, ptr::NonNull};

    use super::{RESERVED, SPAN};

    /// [`RESERVED`] bytes of address space at an address aligned to [`SPAN`], inaccessible.
    pub(super) fn reserve() -> io::Result<NonNull<u8>> {
        let span = SPAN as usize;
        let padded = RESERVED + span;
        // SAFETY: a fresh anonymous mapping at an address the kernel picks: it overlaps
        // nothing the program holds.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                padded,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let addr = raw as usize;
        let aligned = addr.next_multiple_of(span);
        let head = aligned - addr;
        let tail = padded - head - RESERVED;
        if head != 0 {
            // SAFETY: `[raw, aligned)` is the front of the mapping just made, which nothing
            // else knows of.
            unsafe { libc::munmap(raw, head) };
        }
        if tail != 0 {
            // SAFETY: `[aligned + RESERVED, raw + padded)` is the back of the same mapping.
            unsafe { libc::munmap((aligned + RESERVED) as *mut libc::c_void, tail) };
        }
        NonNull::new(aligned as *mut u8).ok_or_else(|| io::Error::other("mmap gave address 0"))
    }

    /// Makes `[ptr, ptr + len)` readable and writable, zeroed where it was never mapped.
    ///
    /// # Safety
    ///
    /// The range is page-aligned and inside a live reservation of [`reserve`].
    pub(super) unsafe fn commit(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
        // SAFETY: the caller says the range is inside our live reservation, which nothing else
        // maps; changing its protection touches no other memory.
        let rc = unsafe { libc::mprotect(ptr.as_ptr().cast(), len, libc::PROT_READ | libc::PROT_WRITE) };
        if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    /// Makes `[ptr, ptr + len)` read-only.
    ///
    /// # Safety
    ///
    /// As for [`commit`], and the range is mapped.
    pub(super) unsafe fn protect_readonly(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
        // SAFETY: as for `commit`.
        let rc = unsafe { libc::mprotect(ptr.as_ptr().cast(), len, libc::PROT_READ) };
        if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    /// Gives `[ptr, ptr + len)` back and makes it inaccessible; mapped again, it reads zeros.
    ///
    /// # Safety
    ///
    /// As for [`commit`], and nothing reads or writes the range until it is mapped again.
    pub(super) unsafe fn decommit(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
        // SAFETY: the caller says the range is inside our live reservation and unused; a fixed
        // mapping over it replaces only those pages.
        let got = unsafe {
            libc::mmap(
                ptr.as_ptr().cast(),
                len,
                libc::PROT_NONE,
                libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if got == libc::MAP_FAILED { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Releases a whole reservation.
    ///
    /// # Safety
    ///
    /// `ptr` is what [`reserve`] gave, and nothing uses the range again.
    pub(super) unsafe fn release(ptr: NonNull<u8>) {
        // SAFETY: the caller hands over the whole reservation, which nothing uses any more.
        unsafe { libc::munmap(ptr.as_ptr().cast(), RESERVED) };
    }
}

#[cfg(windows)]
mod os {
    use std::{io, ptr::NonNull};

    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READONLY,
        PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualProtect,
    };

    use super::{RESERVED, SPAN};

    /// How often another thread may take the aligned range between our release of the padded
    /// reservation and our reservation of the range, before it is an error.
    const TRIES: u32 = 16;

    /// [`RESERVED`] bytes at an address aligned to [`SPAN`]: Windows can't release part of a
    /// reservation, so a padded one finds the address, is released, and the aligned range is
    /// reserved at it.
    pub(super) fn reserve() -> io::Result<NonNull<u8>> {
        let span = SPAN as usize;
        for _ in 0..TRIES {
            // SAFETY: a fresh reservation at an address the kernel picks.
            let raw = unsafe { VirtualAlloc(std::ptr::null(), RESERVED + span, MEM_RESERVE, PAGE_NOACCESS) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let aligned = (raw as usize).next_multiple_of(span);
            // SAFETY: `raw` is the reservation just made, released whole and unused.
            unsafe { VirtualFree(raw, 0, MEM_RELEASE) };
            // SAFETY: a fresh reservation at an address no mapping of ours holds; the kernel
            // refuses it if another thread took the range meanwhile.
            let got = unsafe { VirtualAlloc(aligned as *const _, RESERVED, MEM_RESERVE, PAGE_NOACCESS) };
            if let Some(got) = NonNull::new(got.cast::<u8>()) {
                return Ok(got);
            }
        }
        // alloc: error path
        Err(io::Error::other(format!(
            "another thread took the aligned address range {TRIES} times running"
        )))
    }

    /// # Safety
    ///
    /// The range is page-aligned and inside a live reservation of [`reserve`].
    pub(super) unsafe fn commit(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
        // SAFETY: the caller says the range is inside our live reservation; committing
        // committed pages leaves them as they are.
        let got = unsafe { VirtualAlloc(ptr.as_ptr().cast(), len, MEM_COMMIT, PAGE_READWRITE) };
        if got.is_null() { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// # Safety
    ///
    /// As for [`commit`], and the range is committed.
    pub(super) unsafe fn protect_readonly(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
        let mut old = 0;
        // SAFETY: as for `commit`; `old` is a live local to write.
        let ok = unsafe { VirtualProtect(ptr.as_ptr().cast(), len, PAGE_READONLY, &mut old) };
        if ok != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    /// # Safety
    ///
    /// As for [`commit`], and nothing reads or writes the range until it is committed again.
    pub(super) unsafe fn decommit(ptr: NonNull<u8>, len: usize) -> io::Result<()> {
        // SAFETY: the caller says the range is inside our live reservation and unused.
        let ok = unsafe { VirtualFree(ptr.as_ptr().cast(), len, MEM_DECOMMIT) };
        if ok != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    /// # Safety
    ///
    /// `ptr` is what [`reserve`] gave, and nothing uses the range again.
    pub(super) unsafe fn release(ptr: NonNull<u8>) {
        // SAFETY: the caller hands over the whole reservation, which nothing uses any more.
        unsafe { VirtualFree(ptr.as_ptr().cast(), 0, MEM_RELEASE) };
    }
}

#[cfg(not(any(unix, windows)))]
mod os {
    use std::{io, ptr::NonNull};

    fn none() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "this target has no virtual memory to reserve 4 GiB of")
    }

    pub(super) fn reserve() -> io::Result<NonNull<u8>> {
        Err(none())
    }

    pub(super) unsafe fn commit(_: NonNull<u8>, _: usize) -> io::Result<()> {
        Err(none())
    }

    pub(super) unsafe fn protect_readonly(_: NonNull<u8>, _: usize) -> io::Result<()> {
        Err(none())
    }

    pub(super) unsafe fn decommit(_: NonNull<u8>, _: usize) -> io::Result<()> {
        Err(none())
    }

    pub(super) unsafe fn release(_: NonNull<u8>) {}
}

#[cfg(test)]
mod tests;
