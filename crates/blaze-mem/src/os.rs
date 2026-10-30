//! The kernel's virtual memory: reserve address space, commit pages of it, give pages back,
//! release it. Linux and macOS through `mmap`/`mprotect`/`madvise`, Windows through
//! `VirtualAlloc`/`VirtualFree`. Every other function of the crate reaches the kernel through
//! these. Elsewhere (wasm32) there is no address space to reserve, and a reservation is
//! memory taken whole.
//!
//! A reservation is `PROT_NONE` (`PAGE_NOACCESS`, `MEM_RESERVE`): it costs address space and a
//! mapping, no memory. Committing makes a prefix readable and writable; Linux backs it with
//! pages on first touch, macOS and Windows also charge it to the commit limit. On Linux a
//! reservation that asks for huge pages is aligned to [`HUGE_PAGE`](crate::HUGE_PAGE) and
//! advised `MADV_HUGEPAGE`, so the kernel backs it with 2 MB pages when transparent huge pages
//! are on; macOS has no huge pages for anonymous memory on arm64, and Windows' large pages
//! must be committed whole at reservation (`MEM_LARGE_PAGES`), which commit-on-growth cannot do,
//! so there a reservation is made of base pages.

use std::{io, ptr::NonNull, sync::atomic};

/// Alignment of every reservation: a huge page on Unix (so a huge region starts on one);
/// Windows' allocation granularity, which `VirtualAlloc` always gives.
#[cfg(unix)]
pub(crate) const RESERVE_ALIGN: usize = crate::HUGE_PAGE;
#[cfg(not(unix))]
pub(crate) const RESERVE_ALIGN: usize = 64 << 10;

static PAGE: atomic::AtomicUsize = atomic::AtomicUsize::new(0);

pub(crate) fn page_size() -> usize {
    let page = PAGE.load(atomic::Ordering::Relaxed);
    if page != 0 {
        return page;
    }
    let page = query_page_size();
    PAGE.store(page, atomic::Ordering::Relaxed);
    page
}

#[cfg(unix)]
fn query_page_size() -> usize {
    // SAFETY: `sysconf` reads a constant of the system; it has no memory preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page).ok().filter(|page| page.is_power_of_two()).unwrap_or(4096)
}

/// Windows' page is 4 KB on every architecture it runs blaze on (x86-64, arm64); without
/// virtual memory it is only the unit regions commit in.
#[cfg(not(unix))]
fn query_page_size() -> usize {
    4096
}

/// Reserves `bytes` (a multiple of [`page_size`]) of address space, aligned to
/// [`RESERVE_ALIGN`], none of it committed. With `huge`, the kernel is asked to back it with
/// huge pages where it can.
#[cfg(unix)]
pub(crate) fn reserve(bytes: usize, huge: bool) -> io::Result<NonNull<u8>> {
    let padded = bytes
        .checked_add(RESERVE_ALIGN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "reservation overflows"))?;
    // SAFETY: a fresh anonymous mapping at an address the kernel picks: it overlaps nothing the
    // program holds.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            padded,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let base = base.cast::<u8>();
    let head = base.align_offset(RESERVE_ALIGN);
    let tail = padded - head - bytes;
    let aligned = base.wrapping_add(head);
    if head != 0 {
        // SAFETY: `[base, aligned)` is the front of the mapping just made, which nothing else
        // knows of.
        let _ = unsafe { libc::munmap(base.cast(), head) };
    }
    if tail != 0 {
        // SAFETY: `[aligned + bytes, base + padded)` is the back of the same fresh mapping.
        let _ = unsafe { libc::munmap(aligned.wrapping_add(bytes).cast(), tail) };
    }
    #[cfg(target_os = "linux")]
    if huge {
        // Advice only: with transparent huge pages off the kernel says EINVAL and the region
        // is made of base pages, which is what the machine offers.
        // SAFETY: `[aligned, aligned + bytes)` is the reservation just made.
        let _ = unsafe { libc::madvise(aligned.cast(), bytes, libc::MADV_HUGEPAGE) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = huge;
    NonNull::new(aligned).ok_or_else(|| io::Error::other("mmap returned null"))
}

#[cfg(windows)]
pub(crate) fn reserve(bytes: usize, _huge: bool) -> io::Result<NonNull<u8>> {
    use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
    // SAFETY: a fresh reservation at an address the kernel picks.
    let base = unsafe { VirtualAlloc(std::ptr::null(), bytes, MEM_RESERVE, PAGE_NOACCESS) };
    NonNull::new(base.cast::<u8>()).ok_or_else(io::Error::last_os_error)
}

/// Without virtual memory (wasm32's linear memory only grows at its end) a reservation is the
/// whole range taken at once, zeroed, from the global allocator: it never moves, as on the
/// kernels, but costs its full size from the start.
#[cfg(not(any(unix, windows)))]
pub(crate) fn reserve(bytes: usize, _huge: bool) -> io::Result<NonNull<u8>> {
    let layout = whole(bytes)?;
    // SAFETY: `layout` has a nonzero size (`whole` refuses zero).
    let base = unsafe { std::alloc::alloc_zeroed(layout) };
    NonNull::new(base).ok_or_else(|| {
        io::Error::new(io::ErrorKind::OutOfMemory, "the reservation is more than memory holds")
    })
}

#[cfg(not(any(unix, windows)))]
fn whole(bytes: usize) -> io::Result<std::alloc::Layout> {
    std::alloc::Layout::from_size_align(bytes.max(1), RESERVE_ALIGN)
        .map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, "reservation overflows"))
}

/// Makes `[ptr, ptr + bytes)` readable and writable.
///
/// # Safety
///
/// The range is page-aligned and inside one reservation of [`reserve`] that is not released.
/// Committing a range that is already committed is allowed and leaves its contents alone.
#[cfg(unix)]
pub(crate) unsafe fn commit(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
    // SAFETY: the caller says the range is inside a live reservation of ours, which nothing
    // else maps; changing its protection touches no other memory.
    let rc = unsafe {
        libc::mprotect(ptr.as_ptr().cast(), bytes, libc::PROT_READ | libc::PROT_WRITE)
    };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(windows)]
pub(crate) unsafe fn commit(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
    // SAFETY: the caller says the range is inside a live reservation of ours; committing
    // committed pages leaves them as they are.
    let got = unsafe { VirtualAlloc(ptr.as_ptr().cast(), bytes, MEM_COMMIT, PAGE_READWRITE) };
    if got.is_null() { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// The whole reservation is memory already.
#[cfg(not(any(unix, windows)))]
pub(crate) unsafe fn commit(_ptr: NonNull<u8>, _bytes: usize) -> io::Result<()> {
    Ok(())
}

/// Gives `[ptr, ptr + bytes)` back to the kernel and makes it inaccessible again, still
/// reserved.
///
/// # Safety
///
/// As for [`commit`], and nothing reads or writes the range until it is committed again.
#[cfg(unix)]
pub(crate) unsafe fn decommit(ptr: NonNull<u8>, bytes: usize, huge: bool) -> io::Result<()> {
    // SAFETY: the caller says the range is inside a live reservation of ours that nothing uses;
    // a fixed mapping over it replaces only those pages.
    let got = unsafe {
        libc::mmap(
            ptr.as_ptr().cast(),
            bytes,
            libc::PROT_NONE,
            libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if got == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    if huge {
        // SAFETY: the same range, mapped afresh just now; the new mapping lost the advice.
        let _ = unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, libc::MADV_HUGEPAGE) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = huge;
    Ok(())
}

#[cfg(windows)]
pub(crate) unsafe fn decommit(ptr: NonNull<u8>, bytes: usize, _huge: bool) -> io::Result<()> {
    use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};
    // SAFETY: the caller says the range is inside a live reservation of ours that nothing uses.
    let ok = unsafe { VirtualFree(ptr.as_ptr().cast(), bytes, MEM_DECOMMIT) };
    if ok != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Keeps the memory and zeroes it, so a range committed again reads zeros as on Linux.
#[cfg(not(any(unix, windows)))]
pub(crate) unsafe fn decommit(ptr: NonNull<u8>, bytes: usize, _huge: bool) -> io::Result<()> {
    // SAFETY: the caller says the range is inside a live reservation of ours that nothing
    // uses; the reservation is memory we own, so it is writable.
    unsafe { ptr.as_ptr().write_bytes(0, bytes) };
    Ok(())
}

/// Lets the kernel take the pages of `[ptr, ptr + bytes)` back while they stay committed: the
/// next touch finds them zeroed (Linux) or unspecified (macOS, Windows).
///
/// # Safety
///
/// As for [`commit`], the range is committed, and nothing holds data in it.
#[cfg(unix)]
pub(crate) unsafe fn purge(ptr: NonNull<u8>, bytes: usize) {
    #[cfg(target_os = "macos")]
    let advice = libc::MADV_FREE_REUSABLE;
    #[cfg(not(target_os = "macos"))]
    let advice = libc::MADV_DONTNEED;
    // Advice: a refusal leaves the pages resident, which costs memory and not correctness.
    // SAFETY: the caller says the committed range holds nothing anyone reads.
    let _ = unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, advice) };
}

#[cfg(windows)]
pub(crate) unsafe fn purge(ptr: NonNull<u8>, bytes: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RESET, PAGE_READWRITE, VirtualAlloc};
    // SAFETY: the caller says the committed range holds nothing anyone reads.
    let _ = unsafe { VirtualAlloc(ptr.as_ptr().cast(), bytes, MEM_RESET, PAGE_READWRITE) };
}

#[cfg(not(any(unix, windows)))]
pub(crate) unsafe fn purge(_ptr: NonNull<u8>, _bytes: usize) {}

/// Releases a whole reservation.
///
/// # Safety
///
/// `ptr` and `bytes` are exactly what [`reserve`] returned and was asked for, and nothing uses
/// the range again.
#[cfg(unix)]
pub(crate) unsafe fn release(ptr: NonNull<u8>, bytes: usize) {
    // SAFETY: the caller hands over the whole reservation, which nothing uses any more.
    let _ = unsafe { libc::munmap(ptr.as_ptr().cast(), bytes) };
}

#[cfg(windows)]
pub(crate) unsafe fn release(ptr: NonNull<u8>, _bytes: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    // SAFETY: the caller hands over the whole reservation, which nothing uses any more.
    let _ = unsafe { VirtualFree(ptr.as_ptr().cast(), 0, MEM_RELEASE) };
}

#[cfg(not(any(unix, windows)))]
pub(crate) unsafe fn release(ptr: NonNull<u8>, bytes: usize) {
    if let Ok(layout) = whole(bytes) {
        // SAFETY: the caller hands over exactly what `reserve` returned for `bytes`, which
        // `reserve` took from the global allocator with this same layout.
        unsafe { std::alloc::dealloc(ptr.as_ptr(), layout) };
    }
}
