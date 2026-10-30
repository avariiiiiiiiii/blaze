//! A [`Driver`] on Linux's io_uring: one ring, shared with the kernel through memory, that the
//! thread which polls fills with requests and reaps completions from. The kernel takes the
//! requests on a thread of its own (`IORING_SETUP_SQPOLL`), so a busy driver submits with no
//! system call; an idle one lets that thread sleep and wakes it when it writes again.
//!
//! Reads, appends, syncs, writes at an offset, whole-file writes and renames are the ring's;
//! timers are a [`Wheel`] whose next deadline ends the ring's wait. A file is opened once,
//! kept open and registered in the ring's table of files, so requests name its slot. A whole
//! file's write is one linked chain (written, synced, renamed over the old, its directory
//! synced), and a rename is linked to its directories' syncs. A large aligned write at an
//! offset bypasses the page cache from a registered buffer. A truncation, which comes only in
//! recovery, is a call made where it is submitted.
//!
//! A spawned process is watched through the ring too: a poll of its pidfd for its exit, its
//! output pipes read and its input written by the ring. A listen's accepts (one multishot
//! request for all of them, each connection put straight into the table), a connect, a
//! connection's receives (one multishot request into a ring of provided buffers) and sends,
//! and a hangup's shutdown are the ring's; mapping a file is a call made where it is
//! submitted. A listing, which may take long, runs on a thread of its own, which wakes the ring
//! through an eventfd it reads.
//!
//! The ring is set up with the system calls themselves (`io_uring_setup`, `io_uring_enter`,
//! `io_uring_register` and `mmap`), the layouts as `linux/io_uring.h` has them. Linux 6.0 or
//! later: each feature's absence is an error naming it and the kernel that has it. Before 6.1
//! `statx` says no alignment for writes that bypass the cache, so none does.

use std::{
    cell::{RefCell, UnsafeCell},
    collections::VecDeque,
    ffi::CStr,
    fmt::Write as _,
    io::{self, Write as _},
    mem::ManuallyDrop,
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        unix::ffi::OsStrExt as _,
    },
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence},
        mpsc,
    },
    time::{Duration, Instant},
};

use blaze_mem::{Handle, Slab, Tag as MemTag, scratch};
use blaze_radix as radix;
use rustc_hash::FxHashMap;

use crate::{
    Command, Completion, Confine, Conn, Done, Driver, Effect, FileName, IoError, Names, Stamp, Tag,
    wheel::{self, Wheel},
};

const OP_FSYNC: u8 = 3;
const OP_WRITE_FIXED: u8 = 5;
const OP_POLL_ADD: u8 = 6;
const OP_ACCEPT: u8 = 13;
const OP_ASYNC_CANCEL: u8 = 14;
const OP_CONNECT: u8 = 16;
const OP_READ: u8 = 22;
const OP_WRITE: u8 = 23;
const OP_SEND: u8 = 26;
const OP_RECV: u8 = 27;
const OP_SHUTDOWN: u8 = 34;
const OP_RENAMEAT: u8 = 35;

const SETUP_SQPOLL: u32 = 1 << 1;
const SETUP_CQSIZE: u32 = 1 << 3;
const SETUP_SUBMIT_ALL: u32 = 1 << 7;
const SETUP_SINGLE_ISSUER: u32 = 1 << 12;
const FEAT_SINGLE_MMAP: u32 = 1 << 0;
const FEAT_NODROP: u32 = 1 << 1;
const FEAT_SQPOLL_NONFIXED: u32 = 1 << 7;
const FEAT_EXT_ARG: u32 = 1 << 8;
const ENTER_GETEVENTS: u32 = 1 << 0;
const ENTER_SQ_WAKEUP: u32 = 1 << 1;
const ENTER_EXT_ARG: u32 = 1 << 3;
/// In the submission ring's flags: the kernel's thread sleeps, to be woken by an enter.
const SQ_NEED_WAKEUP: u32 = 1 << 0;
/// In the submission ring's flags: completions wait in the kernel for room, flushed by an
/// enter that gets events.
const SQ_CQ_OVERFLOW: u32 = 1 << 1;
const OFF_SQ_RING: libc::off_t = 0;
const OFF_SQES: libc::off_t = 0x1000_0000;
/// How long the kernel's submission thread spins for work before it sleeps: an idle daemon
/// burns nothing, a busy one makes no system call to submit.
const SQ_THREAD_IDLE_MS: u32 = 50;

const SQE_FIXED_FILE: u8 = 1 << 0;
const SQE_IO_LINK: u8 = 1 << 2;
const SQE_BUFFER_SELECT: u8 = 1 << 5;
/// In an accept's `ioprio`: one request takes every connection until it is cancelled.
const ACCEPT_MULTISHOT: u16 = 1 << 0;
/// In a receive's `ioprio`: one request receives until it is cancelled or runs out of buffers.
const RECV_MULTISHOT: u16 = 1 << 1;
/// In an accept's file index: the kernel picks a free slot of the table's range for it.
const FILE_INDEX_ALLOC: u32 = u32::MAX;
/// In a completion's flags: it used the provided buffer named in the flags' upper half.
const CQE_F_BUFFER: u32 = 1 << 0;
/// In a completion's flags: the request goes on and will complete again.
const CQE_F_MORE: u32 = 1 << 1;
const CQE_BUFFER_SHIFT: u32 = 16;
/// In a cancel's flags: every request it matches, not the first, and every request at all.
const ASYNC_CANCEL_ALL: u32 = 1 << 0;
const ASYNC_CANCEL_ANY: u32 = 1 << 2;

const REGISTER_FILES2: libc::c_uint = 13;
const REGISTER_FILES_UPDATE2: libc::c_uint = 14;
const REGISTER_BUFFERS2: libc::c_uint = 15;
const REGISTER_PBUF_RING: libc::c_uint = 22;
const REGISTER_FILE_ALLOC_RANGE: libc::c_uint = 25;
const RSRC_REGISTER_SPARSE: u32 = 1 << 0;

/// The most slots of the table of registered files; fewer where `RLIMIT_NOFILE` is lower,
/// which the kernel holds the table to.
const TABLE: u32 = 1 << 16;
/// Registered buffers for writes that bypass the page cache, each a piece of such a write.
const DIRECT_BUFFERS: u16 = 4;
const DIRECT_PIECE: usize = 1 << 20;
/// A `WriteAt` of at least this many bytes, its offset and length aligned to its file
/// system's blocks, bypasses the page cache (`O_DIRECT`).
const DIRECT_LEAST: usize = 1 << 20;
/// The provided buffers a connection's receives take from, and the size of each.
const RECV_BUFFERS: u16 = 64;
const RECV_BUFFER: usize = 16 << 10;
const RECV_GROUP: u16 = 0;
/// Bytes received and not yet asked for, past which a connection stops receiving until they
/// are taken.
const INBOX: usize = 4 << 20;
/// `statx`'s mask bit of the alignment a write that bypasses the cache needs (Linux 6.1).
const STATX_DIOALIGN: u32 = 0x2000;
/// What a [`Stamp`] is made of, as `statx`'s mask asks for it (`linux/stat.h`; `libc` has these
/// only for some C libraries).
const STATX_STAMP: u32 = 0x0001 | 0x0002 | 0x0040 | 0x0080 | 0x0100 | 0x0200;
/// `statx`'s flag to answer from what the kernel has, never asking a network file system's
/// server first.
const AT_STATX_DONT_SYNC: i32 = 0x4000;
/// Names a stamper claims at once: enough to make the claim's atomic add and the reopen of a
/// chunk's first directory a percent or two of them (64 `statx`es are ~30 µs), few enough that
/// the cores' last chunks end within ~30 µs of each other, under 5% of 100 000 names' sweep on
/// 64 cores (docs/v2/plan/lanes/frontend/journal/linux.md, "Speed of light").
const STAMP_CHUNK: usize = 64;

/// Requests the ring holds at once, enough to keep an NVMe queue full; more wait in the
/// driver until there is room.
const ENTRIES: u32 = 4096;
/// Completions it holds unreaped: a multishot request may post many for one request.
const COMPLETIONS: u32 = 4 * ENTRIES;

/// What one read of a spawned process's pipe takes at most: a watch's read's size, so the two
/// share one pool of buffers.
const PIPE: usize = 64 << 10;
const _: () = assert!(PIPE == crate::watch::BUFFER);

/// The `user_data` of a request whose completion is nobody's: its index is above any
/// [`PENDING`] slot, so it names no request.
const NOBODY: u64 = u64::MAX;

/// The most requests in flight or waiting at once, files kept, connections, spawns and
/// watches: room reserved once, committed as it is used.
const PENDING: u32 = 1 << 20;
const KEPT: u32 = 1 << 20;
const CONNS: u32 = 1 << 20;
const SPAWNS: u32 = 1 << 16;
const WATCHES: u32 = 1 << 16;
/// A request's slot is the `user_data`'s bits 2 to 31, beside its link.
const _: () = assert!(PENDING < 1 << 30);
/// Buffers, watches' tables and paths kept from what ended for what starts next.
const SPARES: usize = 64;

/// How long dropping the driver waits for the kernel to give back what it was lent.
const LETTING_GO: Duration = Duration::from_secs(5);

#[repr(C)]
#[derive(Default)]
struct SqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default)]
struct CqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqOffsets,
    cq_off: CqOffsets,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    op_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    splice_fd_in: i32,
    addr3: u64,
    pad: u64,
}

#[repr(C)]
struct Cqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

#[repr(C)]
struct GeteventsArg {
    sigmask: u64,
    sigmask_sz: u32,
    pad: u32,
    ts: u64,
}

#[repr(C)]
struct RsrcRegister {
    nr: u32,
    flags: u32,
    resv2: u64,
    data: u64,
    tags: u64,
}

#[repr(C)]
struct RsrcUpdate2 {
    offset: u32,
    resv: u32,
    data: u64,
    tags: u64,
    nr: u32,
    resv2: u32,
}

#[repr(C)]
struct BufReg {
    ring_addr: u64,
    ring_entries: u32,
    bgid: u16,
    flags: u16,
    resv: [u64; 3],
}

#[repr(C)]
struct FileIndexRange {
    off: u32,
    len: u32,
    resv: u64,
}

/// An entry of a provided buffer ring; the ring's tail is the `resv` of its first entry.
#[repr(C)]
struct ProvidedBuf {
    addr: u64,
    len: u32,
    bid: u16,
    resv: u16,
}

const _: () = assert!(size_of::<Params>() == 120);
const _: () = assert!(size_of::<Sqe>() == 64);
const _: () = assert!(size_of::<Cqe>() == 16);
const _: () = assert!(size_of::<RsrcRegister>() == 32);
const _: () = assert!(size_of::<RsrcUpdate2>() == 32);
const _: () = assert!(size_of::<BufReg>() == 40);
const _: () = assert!(size_of::<FileIndexRange>() == 16);
const _: () = assert!(size_of::<ProvidedBuf>() == 16);

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code, reason = "made by the kernel, read through a pointer")]
struct StatxTime {
    sec: i64,
    nsec: u32,
    _pad: i32,
}

/// `struct statx` of `linux/stat.h`, the same on every architecture and C library, with only
/// what a [`Stamp`] reads named.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code, reason = "made by the kernel, read through a pointer")]
struct Statx {
    mask: u32,
    /// `stx_blksize`, `stx_attributes`, `stx_nlink`, `stx_uid` and `stx_gid`.
    _head: [u32; 6],
    mode: u16,
    _pad: u16,
    ino: u64,
    size: u64,
    /// `stx_blocks` and `stx_attributes_mask`.
    _counts: [u64; 2],
    /// `stx_atime` and `stx_btime`.
    _times: [StatxTime; 2],
    ctime: StatxTime,
    mtime: StatxTime,
    _rest: [u64; 16],
}

const _: () = assert!(size_of::<Statx>() == 256);
const _: () = assert!(std::mem::offset_of!(Statx, mode) == 28);
const _: () = assert!(std::mem::offset_of!(Statx, ino) == 32);
const _: () = assert!(std::mem::offset_of!(Statx, ctime) == 96);
const _: () = assert!(std::mem::offset_of!(Statx, mtime) == 112);

impl Statx {
    /// The stamp [`crate::fs::stamp_of`] makes of a `stat` of the same file; unknown where the
    /// file system did not fill in all of it.
    fn stamp(&self) -> Stamp {
        if self.mask & STATX_STAMP != STATX_STAMP {
            return Stamp::UNKNOWN;
        }
        let ns = |t: &StatxTime| (t.sec as u64).wrapping_mul(1_000_000_000).wrapping_add(u64::from(t.nsec));
        Stamp {
            ino: self.ino,
            size: self.size,
            mtime_ns: ns(&self.mtime),
            ctime_ns: ns(&self.ctime),
            mode: u32::from(self.mode),
        }
    }
}

/// A name's stamp where its `statx` failed with `-res`: gone where nothing is there, unknown
/// otherwise, as [`crate::fs::stamps`] has it.
fn stamp_of_error(res: i32) -> Stamp {
    match -res {
        libc::ENOENT | libc::ENOTDIR => Stamp::GONE,
        _ => Stamp::UNKNOWN,
    }
}

/// `io_uring_register(2)` of `opcode` with `arg`, or an error naming what was asked and the
/// kernel that has it, where this one refuses it as unknown.
fn register<T>(ring: RawFd, opcode: libc::c_uint, arg: *const T, nr: libc::c_uint, what: &str, since: &str) -> io::Result<i64> {
    // SAFETY: `arg` points at what `opcode` reads (and the memory it names), alive for the call.
    let got = unsafe { libc::syscall(libc::SYS_io_uring_register, ring, opcode, arg, nr) };
    if got >= 0 {
        return Ok(got);
    }
    let error = io::Error::last_os_error();
    Err(match error.raw_os_error() {
        Some(libc::EINVAL | libc::EOPNOTSUPP) => io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: the error of a setup that failed.
            format!("io_uring {what}: {error}: Linux {since} or later has it, and this is Linux {}", crate::cgroup::kernel()),
        ),
        // alloc: the error of a setup that failed.
        _ => io::Error::new(error.kind(), format!("io_uring {what}: {error}")),
    })
}

/// An anonymous mapping of `len` bytes, page-aligned, unmapped when dropped.
fn anonymous(len: usize) -> io::Result<Map> {
    // SAFETY: a new private anonymous mapping: no file, no address hint, so it overlaps nothing
    // the program holds; the result is checked against `MAP_FAILED` below.
    let at = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(Map { at: at.cast(), len })
}

/// The table of registered files: requests name a slot of it (`IOSQE_FIXED_FILE`), so the
/// kernel takes no reference to a file per request. The first half is the driver's, for the
/// files it keeps open and the sockets it connects; the second is the kernel's, which accepts
/// put connections into directly.
struct Table {
    ring: Rc<OwnedFd>,
    /// Slots of the driver's half that are free.
    free: RefCell<Vec<u32>>,
    ours: u32,
}

impl Table {
    fn new(ring: &Rc<OwnedFd>) -> io::Result<Rc<Table>> {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `&mut limit` is a live, aligned `rlimit` the call writes and does not keep.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let size = u32::try_from(limit.rlim_cur).unwrap_or(u32::MAX).min(TABLE) & !1;
        if size < 2 {
            // alloc: the error of a setup that failed.
            return Err(io::Error::other(format!("RLIMIT_NOFILE is {}: no table of files fits", limit.rlim_cur)));
        }
        let fd = ring.as_raw_fd();
        let sparse = RsrcRegister { nr: size, flags: RSRC_REGISTER_SPARSE, resv2: 0, data: 0, tags: 0 };
        let nr = size_of::<RsrcRegister>() as libc::c_uint;
        register(fd, REGISTER_FILES2, &raw const sparse, nr, "a sparse table of registered files", "5.19")?;
        let ours = size / 2;
        let range = FileIndexRange { off: ours, len: size - ours, resv: 0 };
        register(fd, REGISTER_FILE_ALLOC_RANGE, &raw const range, 0, "a range of the file table for direct accepts", "6.0")?;
        // alloc: clone: setup, once per driver; `ring` is a reference count.
        Ok(Rc::new(Table { ring: ring.clone(), free: RefCell::new((0..ours).rev().collect()), ours }))
    }

    /// Slot `slot` made `fd`'s file, or emptied with -1.
    fn set(&self, slot: u32, fd: RawFd) -> io::Result<()> {
        let fds = [fd];
        let update = RsrcUpdate2 { offset: slot, resv: 0, data: fds.as_ptr() as u64, tags: 0, nr: 1, resv2: 0 };
        let nr = size_of::<RsrcUpdate2>() as libc::c_uint;
        register(self.ring.as_raw_fd(), REGISTER_FILES_UPDATE2, &raw const update, nr, "updating the file table", "5.13")
            .map(|_| ())
    }

    /// `fd` put in a slot of the driver's half, if one is free.
    fn install(self: &Rc<Table>, fd: OwnedFd) -> io::Result<Fixed> {
        let slot = self.free.borrow_mut().pop();
        if let Some(slot) = slot
            && let Err(error) = self.set(slot, fd.as_raw_fd())
        {
            self.free.borrow_mut().push(slot);
            return Err(error);
        }
        // clone: a reference count.
        Ok(Fixed { fd: Some(fd), slot, table: Rc::clone(self) })
    }

    /// The connection an accept put in slot `slot` of the kernel's half.
    fn accepted(self: &Rc<Table>, slot: u32) -> Fixed {
        // clone: a reference count.
        Fixed { fd: None, slot: Some(slot), table: Rc::clone(self) }
    }
}

/// A file the ring's requests name: its slot of the [`Table`] where it has one, else its
/// descriptor. Kept by the one owner ([`Kept`], [`Connection`]) whose count of requests in
/// flight on it holds it, so its slot is emptied, and may be reused, only once no request can
/// name it any more.
struct Fixed {
    /// None for a connection an accept put straight into the table.
    fd: Option<OwnedFd>,
    /// None only where the driver's half of the table was full: the same file, through its
    /// descriptor, so the same bytes.
    slot: Option<u32>,
    table: Rc<Table>,
}

impl Fixed {
    /// The `fd` and flags of a request on it.
    fn target(&self) -> (RawFd, u8) {
        match (self.slot, &self.fd) {
            (Some(slot), _) => (slot as RawFd, SQE_FIXED_FILE),
            (None, Some(fd)) => (fd.as_raw_fd(), 0),
            (None, None) => unreachable!("a file of no slot has its descriptor"),
        }
    }

    fn fd(&self) -> RawFd {
        self.fd.as_ref().map_or(-1, |it| it.as_raw_fd())
    }
}

impl Drop for Fixed {
    fn drop(&mut self) {
        let Some(slot) = self.slot else { return };
        // Emptying the slot drops the table's reference: a socket accepted into it closes.
        if self.table.set(slot, -1).is_ok() && slot < self.table.ours {
            self.table.free.borrow_mut().push(slot);
        }
    }
}

/// Registered buffers (`IORING_REGISTER_BUFFERS2`), page-aligned, for the pieces of writes
/// that bypass the page cache (`WRITE_FIXED`), which need memory aligned to the device's
/// blocks. Pinned once, so no request pins pages of its own.
struct Registered {
    map: Map,
    free: Vec<u16>,
}

impl Registered {
    fn new(ring: RawFd) -> io::Result<Registered> {
        let map = anonymous(DIRECT_BUFFERS as usize * DIRECT_PIECE)?;
        let iovecs: Vec<libc::iovec> = (0..DIRECT_BUFFERS as usize)
            .map(|n| libc::iovec {
                // SAFETY: within the mapping.
                iov_base: unsafe { map.at.add(n * DIRECT_PIECE) }.cast(),
                iov_len: DIRECT_PIECE,
            })
            .collect(); // alloc: setup, once per driver.
        let buffers = RsrcRegister { nr: DIRECT_BUFFERS.into(), flags: 0, resv2: 0, data: iovecs.as_ptr() as u64, tags: 0 };
        let nr = size_of::<RsrcRegister>() as libc::c_uint;
        register(ring, REGISTER_BUFFERS2, &raw const buffers, nr, "registered buffers", "5.13").map_err(|error| {
            match error.raw_os_error() {
                Some(libc::ENOMEM) => io::Error::new(
                    error.kind(),
                    // alloc: the error of a setup that failed.
                    format!(
                        "{error}: pinning {} MiB of buffers needs RLIMIT_MEMLOCK of that much (ulimit -l) or CAP_IPC_LOCK",
                        DIRECT_BUFFERS as usize * DIRECT_PIECE >> 20
                    ),
                ),
                _ => error,
            }
        })?;
        Ok(Registered { map, free: (0..DIRECT_BUFFERS).rev().collect() }) // alloc: setup, once per driver.
    }

    fn at(&self, index: u16) -> *mut u8 {
        // SAFETY: within the mapping: `index` is one of its buffers.
        unsafe { self.map.at.add(index as usize * DIRECT_PIECE) }
    }
}

/// The buffers a connection's multishot receive takes from (`IORING_REGISTER_PBUF_RING`): the
/// kernel picks one per completion, and each is given back once its bytes are copied out.
struct Provided {
    ring: Map,
    buffers: Map,
    tail: u16,
}

impl Provided {
    fn new(fd: RawFd) -> io::Result<Provided> {
        let ring = anonymous(RECV_BUFFERS as usize * size_of::<ProvidedBuf>())?;
        let buffers = anonymous(RECV_BUFFERS as usize * RECV_BUFFER)?;
        let reg = BufReg {
            ring_addr: ring.at as u64,
            ring_entries: RECV_BUFFERS.into(),
            bgid: RECV_GROUP,
            flags: 0,
            resv: [0; 3],
        };
        register(fd, REGISTER_PBUF_RING, &raw const reg, 1, "a ring of provided buffers", "5.19")?;
        let mut provided = Provided { ring, buffers, tail: 0 };
        for bid in 0..RECV_BUFFERS {
            provided.give(bid);
        }
        Ok(provided)
    }

    /// The bytes the kernel put in buffer `bid`.
    fn bytes(&self, bid: u16, len: usize) -> &[u8] {
        // SAFETY: `bid` is a buffer id the kernel took from this group, below `RECV_BUFFERS`, so
        // the offset is within `buffers` (`RECV_BUFFERS * RECV_BUFFER` bytes); if not, the
        // pointer leaves the mapping.
        let at = unsafe { self.buffers.at.add(bid as usize * RECV_BUFFER) };
        // SAFETY: `at` starts buffer `bid`, `RECV_BUFFER` bytes the kernel wrote for this
        // completion and does not touch again until `give(bid)`, which needs `&mut self` and so
        // cannot run while the slice borrows `self`; if not, the slice aliases a kernel write.
        unsafe { std::slice::from_raw_parts(at, len.min(RECV_BUFFER)) }
    }

    /// Buffer `bid` the kernel's again.
    fn give(&mut self, bid: u16) {
        let mask = RECV_BUFFERS - 1;
        let entry: *mut ProvidedBuf = self.ring.at((self.tail & mask) as u32 * size_of::<ProvidedBuf>() as u32);
        // SAFETY: `bid` is below `RECV_BUFFERS` (from `new`'s loop or a completion of this
        // group), so the offset is within `buffers`.
        let addr = unsafe { self.buffers.at.add(bid as usize * RECV_BUFFER) } as u64;
        // SAFETY: `entry` is slot `tail & mask` of the ring `new` mapped for `RECV_BUFFERS`
        // entries; the kernel does not read it until the tail store below moves past it; if it
        // did, it would read a half-written entry.
        unsafe { (*entry).addr = addr };
        // SAFETY: as above.
        unsafe { (*entry).len = RECV_BUFFER as u32 };
        // SAFETY: as above.
        unsafe { (*entry).bid = bid };
        self.tail = self.tail.wrapping_add(1);
        // SAFETY: the tail is the `resv` of the first entry, the kernel's to read.
        unsafe { (*self.ring.at::<AtomicU16>(14)).store(self.tail, Ordering::Release) };
    }
}

const _: () = assert!(RECV_BUFFERS.is_power_of_two());

/// What `statx` says of a descriptor's file: its device, and the alignment of offset and
/// length a write bypassing the page cache needs (0 where its file system has none).
fn direct_alignment(fd: RawFd) -> io::Result<(u64, u32)> {
    #[repr(C, align(8))]
    struct Statx([u8; 256]);
    let mut statx = Statx([0; 256]);
    // SAFETY: an open descriptor, an empty path with `AT_EMPTY_PATH`, and a buffer of the
    // size of `struct statx` to write.
    let got = unsafe {
        libc::syscall(libc::SYS_statx, fd, c"".as_ptr(), libc::AT_EMPTY_PATH, STATX_DIOALIGN, statx.0.as_mut_ptr())
    };
    if got != 0 {
        return Err(io::Error::last_os_error());
    }
    let word = |at: usize| u32::from_ne_bytes(statx.0[at..at + 4].try_into().expect("four bytes"));
    let device = (u64::from(word(136)) << 32) | u64::from(word(140));
    // `stx_dio_offset_align`, where the mask says it is filled in.
    let align = match word(0) & STATX_DIOALIGN {
        0 => 0,
        _ => word(156),
    };
    Ok((device, align))
}

/// A mapping of the ring's memory, unmapped when dropped.
struct Map {
    at: *mut u8,
    len: usize,
}

impl Map {
    fn new(fd: RawFd, len: usize, offset: libc::off_t) -> io::Result<Map> {
        // SAFETY: a shared mapping of the ring's file at the offset the kernel names for it.
        let at = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                offset,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Map { at: at.cast(), len })
    }

    /// The `T` at `offset`, which the kernel's offsets say is one.
    fn at<T>(&self, offset: u32) -> *mut T {
        debug_assert!(offset as usize + size_of::<T>() <= self.len);
        // SAFETY: every caller passes an offset the kernel gave in `io_uring_params` (`sq_off`,
        // `cq_off`, 0 for the SQE array) or the fixed `resv` offset 14 of a provided-buffer ring,
        // each within the `len` bytes mapped (the debug assertion checks it); if not, the pointer
        // leaves the mapping.
        unsafe { self.at.add(offset as usize).cast() }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: the mapping `new` made, unmapped once.
        unsafe { libc::munmap(self.at.cast(), self.len) };
    }
}

/// The ring: its file, the mappings and where in them the kernel's counters are.
struct Ring {
    sq_head: *const AtomicU32,
    sq_tail: *const AtomicU32,
    sq_flags: *const AtomicU32,
    sq_mask: u32,
    sq_entries: u32,
    sqes: *mut Sqe,
    cq_head: *const AtomicU32,
    cq_tail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const Cqe,
    /// Requests written to the ring since the kernel's thread was last told of them.
    unsubmitted: u32,
    // Dropped after the pointers into them are no longer used.
    _maps: Vec<Map>,
    /// Shared with the [`Table`], whose slots are emptied through it until the last goes.
    fd: Rc<OwnedFd>,
}

impl Ring {
    /// Submitted by a kernel thread of its own (`IORING_SETUP_SQPOLL`), which takes requests
    /// from the ring as they are written, with no system call, and sleeps after
    /// [`SQ_THREAD_IDLE_MS`] of nothing to do. For one issuing thread (a [`Uring`] is not
    /// `Send`; `IORING_SETUP_SINGLE_ISSUER`). With a thread of its own the kernel runs every
    /// completion's work there, so `IORING_SETUP_DEFER_TASKRUN`, `COOP_TASKRUN` and
    /// `TASKRUN_FLAG`, which say when the issuing thread runs it, mean nothing with SQPOLL, and
    /// the kernel refuses them with it.
    fn new(entries: u32, completions: u32) -> io::Result<Ring> {
        let mut params = Params {
            flags: SETUP_SQPOLL | SETUP_CQSIZE | SETUP_SUBMIT_ALL | SETUP_SINGLE_ISSUER,
            sq_thread_idle: SQ_THREAD_IDLE_MS,
            cq_entries: completions,
            ..Params::default()
        };
        // SAFETY: `params` is the `io_uring_params` the call reads and fills.
        let fd = unsafe {
            libc::syscall(libc::SYS_io_uring_setup, entries, &raw mut params as *mut libc::c_void)
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            let errno = error.raw_os_error().unwrap_or(0);
            let disabled = std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
                .ok()
                .and_then(|it| it.trim().parse().ok());
            let kind = match errno {
                libc::ENOSYS | libc::EINVAL => io::ErrorKind::Unsupported,
                _ => error.kind(),
            };
            let why = setup_refused(errno, &error, disabled, &crate::cgroup::kernel());
            return Err(io::Error::new(kind, why));
        }
        // SAFETY: the call returned a file descriptor of its own.
        let fd = Rc::new(unsafe { OwnedFd::from_raw_fd(fd as RawFd) }); // alloc: setup, once per driver.
        let wanted = [
            (FEAT_SINGLE_MMAP, "IORING_FEAT_SINGLE_MMAP", "5.4"),
            (FEAT_NODROP, "IORING_FEAT_NODROP", "5.5"),
            (FEAT_SQPOLL_NONFIXED, "IORING_FEAT_SQPOLL_NONFIXED", "5.11"),
            (FEAT_EXT_ARG, "IORING_FEAT_EXT_ARG", "5.11"),
        ];
        if let Some((_, name, since)) = wanted.iter().find(|(bit, ..)| params.features & bit == 0) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                // alloc: the error of a setup that failed.
                format!("io_uring without {name}: Linux {since} or later has it, and this is Linux {}", crate::cgroup::kernel()),
            ));
        }
        let sq_len = params.sq_off.array as usize + params.sq_entries as usize * size_of::<u32>();
        let cq_len = params.cq_off.cqes as usize + params.cq_entries as usize * size_of::<Cqe>();
        let rings = Map::new(fd.as_raw_fd(), sq_len.max(cq_len), OFF_SQ_RING)?;
        let sqes =
            Map::new(fd.as_raw_fd(), params.sq_entries as usize * size_of::<Sqe>(), OFF_SQES)?;
        let (sq, cq) = (&params.sq_off, &params.cq_off);
        let array: *mut u32 = rings.at(sq.array);
        for index in 0..params.sq_entries {
            // SAFETY: `index < sq_entries` and the kernel's array at `sq.array` has
            // `sq_entries` `u32` slots in `rings`.
            let slot = unsafe { array.add(index as usize) };
            // SAFETY: `slot` is in the mapping, aligned (the kernel's offset) and not yet read by
            // the kernel, which waits for the first tail move.
            unsafe { slot.write(index) };
        }
        // SAFETY: the kernel put the submission ring's mask, a `u32` it never changes, at
        // `sq.ring_mask` of `rings`.
        let sq_mask = unsafe { *rings.at::<u32>(sq.ring_mask) };
        // SAFETY: as above, the completion ring's mask at `cq.ring_mask`.
        let cq_mask = unsafe { *rings.at::<u32>(cq.ring_mask) };
        Ok(Ring {
            sq_head: rings.at(sq.head),
            sq_tail: rings.at(sq.tail),
            sq_flags: rings.at(sq.flags),
            sq_mask,
            sq_entries: params.sq_entries,
            sqes: sqes.at(0),
            cq_head: rings.at(cq.head),
            cq_tail: rings.at(cq.tail),
            cq_mask,
            cqes: rings.at(cq.cqes),
            unsubmitted: 0,
            _maps: vec![rings, sqes], // alloc: setup, once per driver.
            fd,
        })
    }

    /// Writes `sqes` to the ring one after another, all or, where there is no room for all,
    /// none (a linked chain is links of requests next to each other).
    fn push(&mut self, sqes: &[Sqe]) -> bool {
        // SAFETY: `sq_head` points at the kernel's `AtomicU32` in `rings`, mapped while `self`
        // lives; the kernel moves it, so only atomic loads.
        let head = unsafe { (*self.sq_head).load(Ordering::Acquire) };
        // SAFETY: `sq_tail` as above; only this thread writes it.
        let tail = unsafe { (*self.sq_tail).load(Ordering::Relaxed) };
        if tail.wrapping_sub(head) as usize + sqes.len() > self.sq_entries as usize {
            return false;
        }
        for (n, sqe) in sqes.iter().enumerate() {
            let at = tail.wrapping_add(n as u32) & self.sq_mask;
            // SAFETY: `at` is masked by `sq_mask`, so below `sq_entries`, the length of the SQE
            // array.
            let slot = unsafe { self.sqes.add(at as usize) };
            // SAFETY: slots from `tail` are not the kernel's until the tail store below moves
            // past them; if it read one early it would see a torn request.
            unsafe { slot.write(*sqe) };
        }
        // SAFETY: as above; the release publishes the entries with the tail.
        unsafe { (*self.sq_tail).store(tail.wrapping_add(sqes.len() as u32), Ordering::Release) };
        self.unsubmitted += sqes.len() as u32;
        true
    }

    /// The submission ring's flags, read after every write of it is visible to the kernel's
    /// thread (the full fence pairs with the one the thread makes before it sleeps).
    fn flags(&self) -> u32 {
        fence(Ordering::SeqCst);
        // SAFETY: the kernel's counter at the offset it named.
        unsafe { (*self.sq_flags).load(Ordering::Relaxed) }
    }

    /// Wakes the kernel's thread where it sleeps with requests written since, and, with
    /// `wait`, waits up to it for one completion. Completions the kernel held for want of room
    /// are flushed to the ring by any enter that gets events.
    fn enter(&mut self, wait: Option<Duration>) -> io::Result<()> {
        let flags = self.flags();
        // SAFETY: `sq_head` points at the kernel's `AtomicU32` in `rings`, mapped while `self`
        // lives; the kernel moves it, so only atomic loads.
        let head = unsafe { (*self.sq_head).load(Ordering::Acquire) };
        // SAFETY: `sq_tail` as above; only this thread writes it.
        let tail = unsafe { (*self.sq_tail).load(Ordering::Relaxed) };
        let queued = head != tail;
        let wake = queued && flags & SQ_NEED_WAKEUP != 0;
        self.unsubmitted = 0;
        let wait = wait.or((flags & SQ_CQ_OVERFLOW != 0).then_some(Duration::ZERO));
        if wait.is_none() && !wake {
            return Ok(());
        }
        let ts =
            wait.map(|it| Timespec { sec: it.as_secs() as i64, nsec: it.subsec_nanos() as i64 });
        let arg = GeteventsArg {
            sigmask: 0,
            sigmask_sz: 0,
            pad: 0,
            ts: ts.as_ref().map_or(0, |it| it as *const Timespec as u64),
        };
        let (min_complete, mut flags) = match wait {
            Some(_) => (1, ENTER_GETEVENTS | ENTER_EXT_ARG),
            None => (0, ENTER_EXT_ARG),
        };
        if wake {
            flags |= ENTER_SQ_WAKEUP;
        }
        loop {
            // SAFETY: `arg` and the timespec it points at outlive the call. With SQPOLL the
            // kernel's thread submits; the call only wakes it and waits.
            let entered = unsafe {
                libc::syscall(
                    libc::SYS_io_uring_enter,
                    self.fd.as_raw_fd(),
                    0,
                    min_complete,
                    flags,
                    &raw const arg,
                    size_of::<GeteventsArg>(),
                )
            };
            if entered >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                // The wait ended with nothing done.
                Some(libc::ETIME) => return Ok(()),
                // Completions to reap first.
                Some(libc::EBUSY | libc::EAGAIN) => return Ok(()),
                _ => return Err(error),
            }
        }
    }

    /// Every completion the kernel has posted: its `user_data`, its result and its flags
    /// ([`CQE_F_MORE`], [`CQE_F_BUFFER`] and the buffer's id).
    fn reap(&mut self, mut each: impl FnMut(u64, i32, u32)) {
        // SAFETY: `cq_head` points at the kernel's `AtomicU32` in `rings`; only this thread
        // writes it.
        let mut head = unsafe { (*self.cq_head).load(Ordering::Relaxed) };
        // SAFETY: `cq_tail` as above; the kernel moves it, and the acquire pairs with its release
        // of the entries.
        let tail = unsafe { (*self.cq_tail).load(Ordering::Acquire) };
        while head != tail {
            // SAFETY: masked by `cq_mask`, so below the completion ring's `cq_entries`.
            let at = unsafe { self.cqes.add((head & self.cq_mask) as usize) };
            // SAFETY: entries between head and tail are posted (the acquire above) and the kernel
            // does not reuse one until the head store below passes it; the reference is dropped
            // before that store.
            let cqe = unsafe { &*at };
            let (user_data, res, flags) = (cqe.user_data, cqe.res, cqe.flags);
            head = head.wrapping_add(1);
            // SAFETY: as above; the entry is the kernel's again once the head is past it.
            unsafe { (*self.cq_head).store(head, Ordering::Release) };
            each(user_data, res, flags);
        }
    }
}

/// What blaze says when `io_uring_setup` fails with `errno`: what failed, the likely cause and
/// the fix. `disabled` is `/proc/sys/kernel/io_uring_disabled` (absent before Linux 6.6),
/// `kernel` the running kernel's release. On Linux the build runs on io_uring and on nothing
/// else, so this is where it stops.
fn setup_refused(errno: i32, error: &io::Error, disabled: Option<u8>, kernel: &str) -> String {
    let name = match errno {
        libc::ENOSYS => "ENOSYS",
        libc::EPERM => "EPERM",
        libc::EACCES => "EACCES",
        libc::EINVAL => "EINVAL",
        libc::ENOMEM => "ENOMEM",
        libc::EMFILE => "EMFILE",
        libc::ENFILE => "ENFILE",
        _ => "",
    };
    let seccomp = "a seccomp policy that refuses io_uring_setup, such as Docker's default profile \
                   (Docker 25 and later) or a systemd unit's SystemCallFilter";
    let allow = "run under a seccomp profile that allows io_uring_setup, io_uring_enter and \
                 io_uring_register (Docker: --security-opt seccomp=<that profile>)";
    // alloc: the error of a setup that failed, once; the rest writes into it.
    let mut out = String::with_capacity(512);
    out.push_str("blaze runs its build on io_uring on Linux and cannot start without it: io_uring_setup failed with ");
    _ = match name {
        "" => write!(out, "errno {errno} ({error}). Likely cause: "),
        name => write!(out, "{name} ({error}). Likely cause: "),
    };
    _ = match (errno, disabled) {
        (libc::EPERM | libc::EACCES, Some(2)) => write!(
            out,
            "the sysctl kernel.io_uring_disabled is 2, which disables io_uring for every process. \
             Fix: sysctl -w kernel.io_uring_disabled=0 as root (and set it in /etc/sysctl.d to keep it)"
        ),
        (libc::EPERM | libc::EACCES, Some(1)) => write!(
            out,
            "the sysctl kernel.io_uring_disabled is 1, which allows io_uring only to processes \
             in the group kernel.io_uring_group names or with CAP_SYS_ADMIN. \
             Fix: sysctl -w kernel.io_uring_disabled=0 as root, or add this user to the group \
             kernel.io_uring_group names"
        ),
        (libc::EPERM | libc::EACCES, _) => write!(out, "{seccomp}. Fix: {allow}"),
        (libc::ENOSYS, _) => write!(
            out,
            "this kernel (Linux {kernel}) has no io_uring: older than 5.1 or built without \
             CONFIG_IO_URING; or {seccomp}, answering ENOSYS. \
             Fix: run on Linux 6.0 or later built with io_uring, or {allow}"
        ),
        (libc::EINVAL, _) => write!(
            out,
            "this kernel (Linux {kernel}) lacks IORING_SETUP_SQPOLL with \
             IORING_SETUP_SINGLE_ISSUER, which Linux 6.0 and later have. \
             Fix: run on Linux 6.0 or later"
        ),
        (libc::ENOMEM, _) => write!(
            out,
            "the locked-memory limit (RLIMIT_MEMLOCK), which kernels before 5.12 charge a ring \
             to, is too small, or the machine is out of memory. \
             Fix: raise the limit (ulimit -l unlimited, or LimitMEMLOCK= for a systemd unit), or run \
             on Linux 6.0 or later"
        ),
        (libc::EMFILE | libc::ENFILE, _) => write!(
            out,
            "no file descriptor is left for the ring, in this process or the system. \
             Fix: raise the open-file limit (ulimit -n)"
        ),
        _ => write!(
            out,
            "an answer io_uring_setup is not known to give on Linux {kernel}. \
             Fix: check that the kernel is Linux 6.0 or later, that kernel.io_uring_disabled is 0, \
             and {allow}"
        ),
    };
    out.push('.');
    out
}

/// A request in flight, in its slot of [`Uring::pending`], which never moves (a [`Slab`]'s
/// entries are in reserved memory). Every buffer the kernel is given is on the heap, or in the
/// slot itself only for a request of one completion (a connect's address), so taking an `Op`
/// out of its slot while one of its completions is stepped ([`Uring::completed`]) moves nothing
/// the kernel may still read: it is put back before the request is asked again. A file it names
/// is held by its owner's count of requests on it ([`Kept::users`], [`Connection::users`]), so
/// neither the file's slot nor its descriptor is let go while the kernel may still be given the
/// request.
enum Op {
    /// Into `bytes` from `at`, `got` of `bytes.len()` read so far.
    Read {
        file: Handle<Kept>,
        at: u64,
        bytes: Vec<u8>,
        got: usize,
    },
    /// `bytes` at the end of the file, `put` of them written so far.
    Append {
        file: Handle<Kept>,
        bytes: Vec<u8>,
        put: usize,
    },
    /// `bytes` at `at` of a file, through a descriptor of its own: the one kept for the file
    /// appends wherever it is told to write. With `direct` the descriptor bypasses the page
    /// cache, and each piece is copied into the registered buffer `buffer` and written from
    /// there; until one is free, it waits.
    WriteAt {
        fd: OwnedFd,
        at: u64,
        bytes: Vec<u8>,
        put: usize,
        direct: bool,
        buffer: Option<u16>,
    },
    Sync {
        file: Handle<Kept>,
    },
    /// A whole file as one chain of linked requests: written to the temporary, synced, renamed
    /// over the path, and its directory `dir` synced. `names` holds the temporary's path and
    /// the path, each NUL-terminated, the second from `to`: a buffer of the driver's
    /// ([`Uring::paths`]), on the heap, which the kernel reads while the rename is in flight. A
    /// link that fails cancels those after it; a short write asks the chain again from where it
    /// stopped.
    Write {
        fd: OwnedFd,
        dir: OwnedFd,
        bytes: Vec<u8>,
        put: usize,
        names: Vec<u8>,
        to: usize,
        file: FileName,
        chain: Chain,
    },
    /// The first of `names` renamed over the second (from `to`), as for [`Op::Write`], then
    /// each of `dirs` there is synced, linked.
    Rename {
        names: Vec<u8>,
        to: usize,
        dirs: [Option<OwnedFd>; 2],
        chain: Chain,
    },
    /// A read of a watch's events, asked again after each until the watch is stopped.
    Watch {
        watch: Handle<Watching>,
    },
    /// Stops the request whose `user_data` is `target`; its own completion is nobody's.
    Cancel {
        target: u64,
    },
    /// The exit of the process of `spawn`: a poll of the pidfd its [`Spawning`] keeps open for
    /// as long as any request of it is in flight.
    Exit {
        spawn: Handle<Spawning>,
        pidfd: RawFd,
    },
    /// One of a spawn's output pipes, read until it ends, into a buffer of
    /// [`Uring::buffers`].
    Output {
        spawn: Handle<Spawning>,
        fd: OwnedFd,
        buffer: Vec<u8>,
        stderr: bool,
    },
    /// A spawn's input, `put` of it written; its pipe is closed when this ends.
    Input {
        spawn: Handle<Spawning>,
        fd: OwnedFd,
        bytes: Vec<u8>,
        put: usize,
    },
    /// An accept of a listen's, asked again after each until the listen is stopped. The
    /// kernel puts each connection straight into a slot of its half of the [`Table`].
    Accept {
        listener: crate::net::Listener,
        stopping: bool,
    },
    /// The kernel reads the address from `at`, in this request's slot, until it completes:
    /// once, so the slot is not left before (see [`Op`]).
    Connect {
        socket: Option<Fixed>,
        at: crate::net::SockAddr,
    },
    /// The one receive of `conn`, multishot: each completion's bytes are in a buffer of the
    /// [`Provided`] ring, copied into the connection's inbox and the buffer given back.
    Recv {
        conn: Conn,
    },
    /// A send on `conn`, `put` of `bytes` sent so far.
    Send {
        conn: Conn,
        bytes: Vec<u8>,
        put: usize,
    },
    /// `conn` shut both ways, for a hangup.
    Shutdown {
        conn: Conn,
    },
    /// A read of the eventfd that threads doing work off the ring bump once they have sent
    /// its completion; in flight while any such work is.
    Wake {
        fd: RawFd,
        /// [`Offload::count`], held while the kernel may write into it.
        count: Rc<UnsafeCell<[u8; 8]>>,
    },
    /// In its slot while its completion is stepped; never asked of the kernel.
    Stepping,
}

impl Op {
    /// The completions the kernel still owes it: one per link of a chain not yet in.
    fn owed(&self) -> usize {
        match self {
            Op::Write { chain, .. } => chain.owed(WRITE_LINKS),
            Op::Rename { chain, dirs, .. } => chain.owed(1 + dirs.iter().flatten().count()),
            _ => 1,
        }
    }
}

/// A watch's read and what it reads for, in [`Uring::watchings`], which is not freed while the
/// kernel may still write into `buffer` (see `Drop`).
struct Watching {
    tree: crate::watch::Tree,
    /// A buffer of [`Uring::buffers`].
    buffer: Vec<u8>,
    stopping: bool,
    /// Flushes waiting for this read, cancelled for them, to complete: the descriptor is
    /// drained then, while the ring's kernel thread holds no read of it.
    flushes: Vec<Tag>,
    /// The watched directory of an [`Effect::Journal`], whose batches are told as
    /// [`Done::Journaled`]; `None` for an [`Effect::Watch`].
    journal: Option<FileName>,
}

/// A file opened for reading and appending, kept open: under its name in [`Uring::names`]
/// while `named`, and kept while any request in flight uses it.
struct Kept {
    fixed: Fixed,
    users: u32,
    named: bool,
}

/// The links of a whole file's write: the write, its sync, the rename and its directory's sync.
const WRITE_LINKS: usize = 4;

/// The results of a linked chain's requests, by link, as their completions come in any order.
#[derive(Default)]
struct Chain {
    results: [Option<i32>; 4],
}

impl Chain {
    /// The result of link `link`; once all `links` are in, every result, and the chain is
    /// empty for its next asking.
    fn take(&mut self, link: usize, res: i32, links: usize) -> Option<[i32; 4]> {
        self.results[link] = Some(res);
        if self.results[..links].iter().any(Option::is_none) {
            return None;
        }
        Some(std::mem::take(&mut self.results).map(|it| it.unwrap_or(0)))
    }

    fn owed(&self, links: usize) -> usize {
        self.results[..links].iter().filter(|it| it.is_none()).count()
    }
}

/// A request's handle: the `user_data` its completions carry names its slot and the
/// generation the slot had, so a completion of a request that ended, whose slot holds another
/// since, is told from that one's and dropped.
type Id = Handle<Pending>;

/// The `user_data` of link `link` of the request `id`, a request of no chain its link 0: the
/// generation in the high half, the slot and the link in the low.
fn data(id: Id, link: usize) -> u64 {
    debug_assert!(id.index() < PENDING && link < 4);
    u64::from(id.generation()) << 32 | u64::from(id.index()) << 2 | link as u64
}

/// The request and link of a `user_data` [`data`] made; any other names no request.
fn of_data(user_data: u64) -> (Id, usize) {
    let index = (user_data as u32) >> 2;
    (Handle::from_bits(user_data >> 32 << 32 | u64::from(index)), (user_data & 3) as usize)
}

/// The handle a [`Conn`] is.
fn conn_of(conn: Conn) -> Handle<Connection> {
    let handle = Handle::from_bits(conn.0);
    // The slab takes an odd generation to mean its entry holds a value, so bits it never
    // issued (an even generation, which a free slot may have) become a handle past every slot.
    if handle.generation() % 2 == 1 { handle } else { Handle::from_bits(u64::MAX) }
}

const _: () = assert!(CONNS < u32::MAX as usize, "u32::MAX is no connection's index");

/// The tag of a request that is no effect's own (a receive, a cancel).
const NOBODYS: Tag = Tag(u64::MAX);

/// Work done on threads of its own rather than through the ring (a listing, which may take
/// long): their completions, and the eventfd that wakes the ring when one is sent.
struct Offload {
    sender: mpsc::Sender<Completion>,
    receiver: mpsc::Receiver<Completion>,
    /// Shared with the threads, so a write of one that ends after the driver is dropped
    /// still reaches an open descriptor.
    wake: Arc<OwnedFd>,
    /// Work handed to threads whose completion has not been received.
    out: usize,
    /// The request reading `wake`, while there is one.
    reading: Option<Id>,
    /// What each read of `wake` reads into, never read itself: one for the driver's life, so
    /// a wait allocates nothing. Kept alive past the driver by a request the kernel keeps.
    count: Rc<UnsafeCell<[u8; 8]>>,
}

impl Offload {
    fn new() -> io::Result<Offload> {
        // SAFETY: no pointers.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let (sender, receiver) = mpsc::channel();
        // SAFETY: the call returned a descriptor of its own.
        let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }); // alloc: setup, once per driver.
        // alloc: setup, once per driver.
        let count = Rc::new(UnsafeCell::new([0; 8]));
        Ok(Offload { sender, receiver, wake, out: 0, reading: None, count })
    }
}

struct Pending {
    tag: Tag,
    op: Op,
    /// Given to the kernel before the ring failed: kept where it is, never freed or asked
    /// again, so the kernel writes into nothing else; its completions are dropped.
    abandoned: bool,
}

/// What became of a request whose completion was reaped.
enum Step {
    /// The rest of it asked for again.
    Again,
    /// `done` told, and it asked for again: a watch's change, a listen's accept.
    Goes(Done),
    /// Kept as it is, not asked again: a chain with links still to complete, or a multishot
    /// request the kernel still holds.
    Held,
    /// It ended, with its effect's completion if it has one of its own.
    Ends(Option<Done>),
}

/// A spawn in flight: its process, until it has exited and both its pipes have ended.
struct Spawning {
    tag: Tag,
    pid: libc::pid_t,
    /// What a kill signals: the process itself, whatever its pid names by then.
    pidfd: OwnedFd,
    finish: crate::process::Finish,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Its requests in flight: its exit, its output pipes and its input.
    parts: usize,
    exited: bool,
    /// Why it completes failed, whatever it exits with.
    failed: Option<String>,
}

impl Spawning {
    /// Its completion, once its exit and both its pipes have ended: reaped, and made the
    /// [`crate::Exit`] its [`crate::process::Finish`] says.
    fn end(self) -> Done {
        let Spawning { pid, pidfd, finish, stdout, stderr, failed, .. } = self;
        // alloc: the error of a spawn that failed.
        let failed = failed.map(|why| format!("{}: {why}", finish.command.program.display()));
        // Reaps it, and removes what was made for it even where it failed whatever it says.
        let exited = finish.exited(pid, stdout, stderr);
        drop(pidfd);
        match (exited, failed) {
            (_, Some(why)) => Done::Failed(IoError(why)),
            // alloc: `Done::Exited`'s own box, one per spawn.
            (Ok(exit), None) => Done::Exited(Box::new(exit)),
            // alloc: the error of a spawn that failed.
            (Err(error), None) => Done::Failed(IoError(error.to_string())),
        }
    }
}

/// A connection a listen accepted or a connect made, in [`Uring::conns`]; its [`Conn`] is
/// its handle's bits, so one hung up and let go is told from the next in its slot.
struct Connection {
    socket: Fixed,
    /// Requests in flight on it (its receive, sends, its shutdown): it is let go once none is
    /// and it is hung up.
    users: usize,
    hung: bool,
    /// What its receive took and no `Recv` has yet, in order.
    inbox: VecDeque<u8>,
    /// The `Recv`s waiting for bytes, in the order they were submitted, with their most.
    recvs: VecDeque<(Tag, usize)>,
    /// Its multishot receive, while the kernel holds one.
    receiving: Option<Id>,
    /// Its receive is being cancelled: the inbox holds [`INBOX`] bytes no `Recv` took.
    pausing: bool,
    /// How its receiving ended: the peer hung up (or reset it), or why it failed.
    ended: Option<Result<(), String>>,
}

/// An [`Effect::Stamp`] on the [`Stampers`]: its names claimed [`STAMP_CHUNK`] at a time, each
/// chunk's stamps written into `out` by the stamper that claimed it. The stamper that finishes
/// the last chunk sends the completion and wakes the ring.
struct Job {
    tag: Tag,
    root: OwnedFd,
    names: Arc<Names>,
    chunks: usize,
    /// The next chunk to claim.
    next: AtomicUsize,
    /// Chunks whose stamps are all written.
    finished: AtomicUsize,
    /// One stamp per name, written through `base`.
    out: UnsafeCell<Vec<Stamp>>,
    base: *mut Stamp,
    sender: mpsc::Sender<Completion>,
    wake: Arc<OwnedFd>,
}

// SAFETY: `out` is written through `base` at disjoint chunks, each only by the one thread whose
// `fetch_add` on `next` claimed it, and taken whole only by the thread whose `fetch_add` on
// `finished` counted the last chunk, after every writer's release (`Job::run`). Every other
// field is `Sync`.
unsafe impl Sync for Job {}
// SAFETY: as above; `base` points into `out`'s heap buffer, which no move of the job moves.
unsafe impl Send for Job {}

impl Job {
    fn new(tag: Tag, root: OwnedFd, names: Arc<Names>, offload: &Offload) -> Job {
        // alloc: the sweep's result, one per sweep, handed to its asker.
        let mut out = vec![Stamp::UNKNOWN; names.len()];
        let base = out.as_mut_ptr();
        Job {
            tag,
            root,
            chunks: names.len().div_ceil(STAMP_CHUNK),
            names,
            next: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            out: UnsafeCell::new(out),
            base,
            sender: offload.sender.clone(), // clone: a channel's handle, one per sweep.
            wake: Arc::clone(&offload.wake),
        }
    }

    /// Claims chunks until none is left, stamping each; whoever finishes the last completes
    /// the job. `parent` is the caller's buffer for a directory's name.
    fn run(&self, parent: &mut Vec<u8>) {
        loop {
            let chunk = self.next.fetch_add(1, Ordering::Relaxed);
            if chunk >= self.chunks {
                return;
            }
            let from = chunk * STAMP_CHUNK;
            let to = (from + STAMP_CHUNK).min(self.names.len());
            stamp_names(self.root.as_raw_fd(), &self.names, from..to, parent, |at, stamp| {
                // SAFETY: `at` is below `names.len()`, the length of `out`'s buffer.
                let slot = unsafe { self.base.add(at) };
                // SAFETY: `at` is in the chunk this thread alone claimed.
                unsafe { slot.write(stamp) };
            });
            if self.finished.fetch_add(1, Ordering::AcqRel) + 1 == self.chunks {
                // SAFETY: every chunk is finished, each writer's release before this acquire,
                // and no thread writes or reads `out` after.
                let out = std::mem::take(unsafe { &mut *self.out.get() });
                if self.sender.send(Completion { tag: self.tag, done: Done::Stamped(out) }).is_ok() {
                    wake_ring(&self.wake);
                }
            }
        }
    }
}

/// Threads, one per core, that stamp [`Effect::Stamp`]'s names with `statx` calls of their own.
/// The ring would prepare each `IORING_OP_STATX` on its one submission thread and run it on a
/// worker after, two handoffs per name that take longer than the `statx` itself
/// (docs/v2/plan/lanes/frontend/journal/linux.md, "Speed of light"). A job reaches stamper 0,
/// and stamper `i` hands it on to `2i + 1` and `2i + 2` before it starts, as far as the job has
/// chunks for: every core is at work after log2 of them wake-ups, none from the ring's thread
/// but the first. [`Effect::Entries`]' listings go to one stamper each, in turn, where a thread
/// of their own would cost as much to start as a listing of a hundred names.
struct Stampers {
    inboxes: Arc<[crossbeam_channel::Sender<ToStamp>]>,
    /// The stamper the next listing goes to.
    next: std::cell::Cell<usize>,
}

/// What a stamper is sent.
enum ToStamp {
    Sweep(Arc<Job>),
    /// A listing, for the stamper it is sent to alone.
    List(List),
    End,
}

/// An [`Effect::Entries`] on a stamper.
struct List {
    tag: Tag,
    root: Arc<Path>,
    dir: FileName,
    sender: mpsc::Sender<Completion>,
    wake: Arc<OwnedFd>,
}

impl List {
    fn run(self) {
        let done = match crate::fs::entries(&self.root.join(self.dir.as_str()), &self.dir) {
            Ok(entries) => Done::Entries(entries),
            // alloc: the error of a listing that failed.
            Err(error) => Done::Failed(IoError(error.to_string())),
        };
        if self.sender.send(Completion { tag: self.tag, done }).is_ok() {
            wake_ring(&self.wake);
        }
    }
}

impl Stampers {
    fn new() -> io::Result<Stampers> {
        let count = std::thread::available_parallelism().map_or(1, |it| it.get());
        let (senders, receivers): (Vec<crossbeam_channel::Sender<ToStamp>>, Vec<crossbeam_channel::Receiver<ToStamp>>) =
            (0..count).map(|_| crossbeam_channel::unbounded()).unzip();
        let stampers = Stampers { inboxes: senders.into(), next: std::cell::Cell::new(0) };
        for (me, inbox) in receivers.into_iter().enumerate() {
            let inboxes = Arc::clone(&stampers.inboxes);
            // Dropping `stampers` on a failure ends those already started.
            std::thread::Builder::new()
                .name(format!("blaze-stamp-{me}")) // alloc: setup, once per stamper.
                .spawn(move || stamper(me, &inbox, &inboxes))?;
        }
        Ok(stampers)
    }

    fn start(&self, job: Arc<Job>) {
        _ = self.inboxes[0].send(ToStamp::Sweep(job));
    }

    fn list(&self, list: List) {
        let at = self.next.get();
        self.next.set((at + 1) % self.inboxes.len());
        _ = self.inboxes[at].send(ToStamp::List(list));
    }
}

impl Drop for Stampers {
    /// Each stamper ends once it has passed the end on, after the work sent before it.
    fn drop(&mut self) {
        _ = self.inboxes[0].send(ToStamp::End);
    }
}

fn stamper(me: usize, inbox: &crossbeam_channel::Receiver<ToStamp>, inboxes: &[crossbeam_channel::Sender<ToStamp>]) {
    // alloc: one buffer per stamper, reused for every directory of every job.
    let mut parent = Vec::with_capacity(libc::PATH_MAX as usize);
    while let Ok(work) = inbox.recv() {
        let wanted = match &work {
            ToStamp::Sweep(job) => job.chunks,
            ToStamp::List(_) => 0,
            ToStamp::End => usize::MAX,
        };
        for next in [2 * me + 1, 2 * me + 2] {
            if next < wanted
                && let Some(it) = inboxes.get(next)
            {
                _ = it.send(match &work {
                    ToStamp::Sweep(job) => ToStamp::Sweep(Arc::clone(job)),
                    _ => ToStamp::End,
                });
            }
        }
        match work {
            ToStamp::Sweep(job) => job.run(&mut parent),
            ToStamp::List(list) => list.run(),
            ToStamp::End => return,
        }
    }
}

/// The stamps of `names[range]`, each handed to `put` with its index. Each is a `statx` of the
/// name's last part relative to its directory, opened once for each run of names in the same
/// one with `O_PATH`, which needs only the search permission a walk of the whole name would.
/// A directory that cannot be opened gives each of its names the stamp a `statx` of the whole
/// name would: gone for `ENOENT` and `ENOTDIR`, unknown otherwise.
fn stamp_names(
    root: RawFd,
    names: &Names,
    range: std::ops::Range<usize>,
    parent: &mut Vec<u8>,
    mut put: impl FnMut(usize, Stamp),
) {
    // The directory open, by its name and a NUL; `parent` empty before the first.
    parent.clear();
    let mut dir: Result<Option<OwnedFd>, Stamp> = Ok(None);
    for at in range {
        let name = names.with_nul(at);
        let (up, leaf) = match name.iter().rposition(|&it| it == b'/') {
            Some(slash) => (&name[..slash], &name[slash + 1..]),
            None => (&name[..0], name),
        };
        if parent.is_empty() || parent[..parent.len() - 1] != *up {
            parent.clear();
            parent.extend_from_slice(up);
            parent.push(0);
            dir = match up.is_empty() {
                true => Ok(None),
                false => open_path(root, parent).map(Some).map_err(stamp_of_error),
            };
        }
        let stamp = match &dir {
            Ok(None) => statx_at(root, leaf),
            Ok(Some(fd)) => statx_at(fd.as_raw_fd(), leaf),
            Err(stamp) => *stamp,
        };
        put(at, stamp);
    }
}

/// The last error of a call of this thread's, as a ring's result is: negative.
fn errno() -> i32 {
    -io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

/// The directory `name` (with its NUL) under `dir`, opened only to name it in calls relative
/// to it; the error as a negative errno.
fn open_path(dir: RawFd, name: &[u8]) -> Result<OwnedFd, i32> {
    debug_assert_eq!(name.last(), Some(&0));
    // SAFETY: an open directory and a NUL-terminated name; the result is a descriptor of its
    // own or -1.
    let fd = unsafe { libc::openat(dir, name.as_ptr().cast(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(errno());
    }
    // SAFETY: the call returned a descriptor of its own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The stamp of `leaf` (with its NUL) under `dir`, not followed through a symlink.
fn statx_at(dir: RawFd, leaf: &[u8]) -> Stamp {
    debug_assert_eq!(leaf.last(), Some(&0));
    let mut statx = std::mem::MaybeUninit::<Statx>::uninit();
    // SAFETY: an open directory, a NUL-terminated name, and room for a `struct statx`.
    let got = unsafe {
        libc::syscall(
            libc::SYS_statx,
            dir,
            leaf.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW | AT_STATX_DONT_SYNC,
            STATX_STAMP,
            statx.as_mut_ptr(),
        )
    };
    if got != 0 {
        return stamp_of_error(errno());
    }
    // SAFETY: the call succeeded, so it filled the `statx`.
    unsafe { statx.assume_init_ref() }.stamp()
}

/// Tells the ring's wait that work off the ring has sent its completion.
fn wake_ring(wake: &OwnedFd) {
    let one = 1u64.to_ne_bytes();
    // SAFETY: eight bytes, which an eventfd write takes, of an open eventfd.
    unsafe { libc::write(wake.as_raw_fd(), one.as_ptr().cast(), one.len()) };
}

pub struct Uring {
    start: Instant,
    /// Shared with the work done off the ring, so handing it out is a count, not a copy.
    root: Arc<Path>,
    ring: Ring,
    table: Rc<Table>,
    registered: Registered,
    /// Not freed while the kernel may still write into it (see `Drop`).
    provided: ManuallyDrop<Provided>,
    /// Each file opened, for reading and appending, and the name each is kept under.
    kept: Slab<Kept>,
    names: FxHashMap<FileName, Handle<Kept>>,
    /// Every request in flight or waiting, by the handle its `user_data` carries ([`data`]).
    /// Not freed while the kernel may still read or write what a request holds (see `Drop`).
    pending: ManuallyDrop<Slab<Pending>>,
    /// Requests for which the ring had no room.
    waiting: VecDeque<Id>,
    /// Writes bypassing the page cache waiting for a registered buffer.
    for_buffers: VecDeque<Id>,
    /// Devices whose file system refused `O_DIRECT`: their writes go through the page cache.
    refused: rustc_hash::FxHashSet<u64>,
    /// Writes that bypassed the page cache, ended.
    direct_writes: u64,
    /// Completions to hand out at the next poll, in order.
    ready: Vec<Completion>,
    /// The completions reaped from the ring, kept for the next reap's.
    posted: Vec<(u64, i32, u32)>,
    /// The request reading each watch's events, by the watch's tag.
    watches: FxHashMap<Tag, Id>,
    /// What each watch's read reads for. Not freed while the kernel may still write into a
    /// buffer of one (see `Drop`).
    watchings: ManuallyDrop<Slab<Watching>>,
    /// The request accepting each listen's connections, by the listen's tag.
    listens: FxHashMap<Tag, Id>,
    spawns: Slab<Spawning>,
    /// Each spawn in flight by its tag, for a kill.
    spawning: FxHashMap<Tag, Handle<Spawning>>,
    conns: Slab<Connection>,
    /// By ticks since `start`; a wait of the ring ends at the next one due.
    timers: Wheel<Tag>,
    // dyn: the embedder's sandbox, called once per spawn, which makes a process anyway.
    confine: Option<Arc<dyn Confine>>,
    /// Buffers of [`PIPE`] bytes, a watch's or a spawn's pipe's, kept from what ended for
    /// what starts next.
    buffers: Vec<Vec<u8>>,
    /// The tables of watches that ended, with their lists of flushes, for the next watch.
    trees: Vec<(crate::watch::Spare, Vec<Tag>)>,
    /// The buffers of names of writes and renames that ended, for the next.
    paths: Vec<Vec<u8>>,
    /// Made for the first [`Effect::Stamp`].
    stampers: Option<Stampers>,
    offload: Offload,
    in_flight: usize,
}

/// A temporary's number, counted from the top half so that it never meets one of
/// [`crate::fs::temporary_next_to`]'s, which count from 0.
static TEMPORARY: AtomicU64 = AtomicU64::new(1 << 63);

impl Uring {
    /// A driver whose files are under `root`, or why this kernel has no ring for it.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Uring> {
        let ring = Ring::new(ENTRIES, COMPLETIONS)?;
        let table = Table::new(&ring.fd)?;
        let registered = Registered::new(ring.fd.as_raw_fd())?;
        let provided = ManuallyDrop::new(Provided::new(ring.fd.as_raw_fd())?);
        fn slab<T>(max: u32) -> io::Result<Slab<T>> {
            Slab::new(max, MemTag::Io).map_err(io::Error::other)
        }
        Ok(Uring {
            start: Instant::now(),
            root: Arc::from(root.into()),
            ring,
            table,
            registered,
            provided,
            kept: slab(KEPT)?,
            names: FxHashMap::default(),
            pending: ManuallyDrop::new(slab(PENDING)?),
            // alloc: setup, once per driver: room for as many as the ring holds.
            waiting: VecDeque::with_capacity(ENTRIES as usize),
            for_buffers: VecDeque::new(), // alloc: empty, nothing allocated; reused after.
            refused: rustc_hash::FxHashSet::default(),
            direct_writes: 0,
            ready: Vec::new(), // alloc: empty, nothing allocated; reused after.
            posted: Vec::new(), // alloc: empty, nothing allocated; reused after.
            watches: FxHashMap::default(),
            watchings: ManuallyDrop::new(slab(WATCHES)?),
            listens: FxHashMap::default(),
            spawns: slab(SPAWNS)?,
            spawning: FxHashMap::default(),
            conns: slab(CONNS)?,
            timers: Wheel::new(),
            confine: None,
            buffers: Vec::with_capacity(SPARES), // alloc: setup, once per driver.
            trees: Vec::with_capacity(SPARES),   // alloc: setup, once per driver.
            paths: Vec::with_capacity(SPARES),   // alloc: setup, once per driver.
            stampers: None,
            offload: Offload::new()?,
            in_flight: 0,
        })
    }

    /// The driver, making the sandbox of each spawn whose [`crate::Sandbox`] is on with
    /// `confine`.
    // dyn: the embedder's sandbox, set once.
    pub fn confined(mut self, confine: Arc<dyn Confine>) -> Uring {
        self.confine = Some(confine);
        self
    }

    /// Where the listen `listen` listens, while it does: the port a TCP listen of port 0 was
    /// given. `None` for a Unix socket's.
    pub fn local_addr(&self, listen: Tag) -> Option<std::net::SocketAddr> {
        let id = self.listens.get(&listen)?;
        match &self.pending.get(*id)?.op {
            Op::Accept { listener: crate::net::Listener::Tcp(listener), .. } => {
                listener.local_addr().ok()
            }
            _ => None,
        }
    }

    /// Every timer due, fired in the order they were due.
    fn fire_timers(&mut self) {
        let ready = &mut self.ready;
        self.timers.advance(wheel::ticks(self.start.elapsed()), |_, tag| {
            ready.push(Completion { tag, done: Done::Fired });
        });
    }

    /// Forgets the descriptor kept for `name`: the name now is another file.
    fn forget(&mut self, name: &FileName) {
        forget(&mut self.names, &mut self.kept, name);
    }

    /// The file, opened once and put in the driver's half of the table; `make` makes it if
    /// missing, with its directories. Where that half is full, the files kept are let go
    /// (each request in flight holds its own until it ends) to make room.
    fn file(&mut self, name: &FileName, make: bool) -> io::Result<Handle<Kept>> {
        if let Some(&file) = self.names.get(name) {
            return Ok(file);
        }
        let fd = under(&self.root, name, |path| {
            if make && let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            open(path, make)
        })?;
        if self.table.free.borrow().is_empty() {
            for (_, file) in self.names.drain() {
                let kept = self.kept.get_mut(file).expect("named files are kept");
                kept.named = false;
                if kept.users == 0 {
                    self.kept.remove(file);
                }
            }
        }
        let fixed = self.table.install(fd)?;
        let file = self.kept.insert(Kept { fixed, users: 0, named: true });
        // clone: the name once per file opened, as the key it is kept under.
        self.names.insert(name.clone(), file);
        Ok(file)
    }

    /// `file` held by one more request, until [`unkeep`].
    fn hold(&mut self, file: Handle<Kept>) -> Handle<Kept> {
        self.kept.get_mut(file).expect("a file just kept").users += 1;
        file
    }

    fn start(&mut self, tag: Tag, op: Op) -> Id {
        let id = self.pending.insert(Pending { tag, op, abandoned: false });
        self.queue(id);
        id
    }

    /// Writes the request `id` as it now stands to the ring, or keeps it for when there is
    /// room (or, bypassing the page cache, for when a registered buffer is free).
    fn queue(&mut self, id: Id) {
        if !self.buffered(id) {
            self.for_buffers.push_back(id);
            return;
        }
        let mut sqes = [Sqe::default(); WRITE_LINKS];
        let n = self.sqes(id, &mut sqes);
        if !self.waiting.is_empty() || !self.ring.push(&sqes[..n]) {
            self.waiting.push_back(id);
        }
    }

    /// Whether the request `id` has the registered buffer it needs, given one if it needs one
    /// and one is free.
    fn buffered(&mut self, id: Id) -> bool {
        let Some(Pending { op: Op::WriteAt { direct: true, buffer, .. }, .. }) = self.pending.get_mut(id)
        else {
            return true;
        };
        if buffer.is_none() {
            *buffer = self.registered.free.pop();
        }
        buffer.is_some()
    }

    /// The registered buffer of a write that ended free again, for the next that waits.
    fn unbuffer(&mut self, buffer: u16) {
        self.registered.free.push(buffer);
        while let Some(id) = self.for_buffers.pop_front() {
            if self.pending.contains(id) {
                self.queue(id);
                return;
            }
        }
    }

    /// A buffer of [`PIPE`] bytes, kept from one that ended where there is one.
    fn buffer(&mut self) -> Vec<u8> {
        // alloc: a pipe's or a watch's buffer, where none that ended is kept for it.
        self.buffers.pop().unwrap_or_else(|| vec![0; PIPE])
    }

    /// `buffer` kept for the next that needs one, where there is room.
    fn give_back(&mut self, buffer: Vec<u8>) {
        if self.buffers.len() < SPARES && buffer.len() == PIPE {
            self.buffers.push(buffer);
        }
    }

    /// An empty buffer for names, kept from a write or rename that ended where there is one.
    fn path(&mut self) -> Vec<u8> {
        // alloc: a buffer of names, where none that ended is kept for it.
        self.paths.pop().unwrap_or_else(|| Vec::with_capacity(2 * libc::PATH_MAX as usize))
    }

    /// What a request that ended held, let go: the file it used, its buffers kept for the next,
    /// a spawn's part ended, a connection's use.
    fn ended(&mut self, tag: Tag, op: Op) {
        match op {
            Op::Read { file, .. } | Op::Append { file, .. } | Op::Sync { file } => unkeep(&mut self.kept, file),
            Op::Write { names, .. } | Op::Rename { names, .. } => self.give_back_path(names),
            Op::Watch { watch } => {
                let Watching { tree, buffer, mut flushes, .. } =
                    self.watchings.remove(watch).expect("a watch's read's watching");
                self.give_back(buffer);
                let spare = tree.end();
                if self.trees.len() < SPARES {
                    flushes.clear();
                    self.trees.push((spare, flushes));
                }
            }
            Op::Exit { spawn, .. } => self.part_ended(spawn),
            Op::Output { spawn, fd, buffer, .. } => {
                // Closes the pipe before the process is reaped.
                drop(fd);
                self.give_back(buffer);
                self.part_ended(spawn);
            }
            Op::Input { spawn, fd, .. } => {
                drop(fd);
                self.part_ended(spawn);
            }
            Op::Recv { conn } => {
                self.release(conn);
                self.deliver(conn);
            }
            Op::Send { conn, .. } | Op::Shutdown { conn } => self.release(conn),
            Op::WriteAt { direct, buffer, .. } => {
                self.direct_writes += u64::from(direct);
                if let Some(buffer) = buffer {
                    self.unbuffer(buffer);
                }
            }
            Op::Cancel { .. } | Op::Accept { .. } | Op::Connect { .. } | Op::Wake { .. } => {}
            Op::Stepping => unreachable!("{tag:?}: a request ended while it was stepped"),
        }
    }

    /// The requests the request `id` is, as it now stands, into `sqes`: one, or each link of
    /// a chain. How many.
    fn sqes(&self, id: Id, sqes: &mut [Sqe; WRITE_LINKS]) -> usize {
        let pending = self.pending.get(id).expect("a request in flight");
        let user_data = data(id, 0);
        let kept = |file: &Handle<Kept>| self.kept.get(*file).expect("held by its request").fixed.target();
        let socket = |conn: &Conn| self.conns.get(conn_of(*conn)).expect("used by its request").socket.target();
        // SAFETY of the kernel's reads and writes: every buffer and address below stays where
        // it is until the request's completion (see `Op`).
        sqes[0] = match &pending.op {
            Op::Read { file, at, bytes, got } => {
                let (fd, flags) = kept(file);
                Sqe {
                    opcode: OP_READ,
                    flags,
                    fd,
                    off: at + *got as u64,
                    addr: bytes[*got..].as_ptr() as u64,
                    len: (bytes.len() - got).min(u32::MAX as usize) as u32,
                    user_data,
                    ..Sqe::default()
                }
            }
            Op::Append { file, bytes, put } => {
                let (fd, flags) = kept(file);
                Sqe {
                    opcode: OP_WRITE,
                    flags,
                    fd,
                    // The file's position, which is its end: it is open for appending.
                    off: u64::MAX,
                    addr: bytes[*put..].as_ptr() as u64,
                    len: (bytes.len() - put).min(u32::MAX as usize) as u32,
                    user_data,
                    ..Sqe::default()
                }
            }
            Op::WriteAt { fd, at, bytes, put, direct: true, buffer } => {
                let buffer = buffer.expect("a registered buffer, given before it is queued");
                let piece = (bytes.len() - put).min(DIRECT_PIECE);
                let to = self.registered.at(buffer);
                // SAFETY: the buffer is this request's alone, `DIRECT_PIECE` long, and the
                // kernel reads it only for this request, which it is not given before this.
                unsafe { ptr::copy_nonoverlapping(bytes[*put..].as_ptr(), to, piece) };
                Sqe {
                    opcode: OP_WRITE_FIXED,
                    fd: fd.as_raw_fd(),
                    off: at + *put as u64,
                    addr: to as u64,
                    len: piece as u32,
                    buf_index: buffer,
                    user_data,
                    ..Sqe::default()
                }
            }
            Op::WriteAt { fd, at, bytes, put, .. } => Sqe {
                opcode: OP_WRITE,
                fd: fd.as_raw_fd(),
                off: at + *put as u64,
                addr: bytes[*put..].as_ptr() as u64,
                len: (bytes.len() - put).min(u32::MAX as usize) as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Sync { file } => {
                let (fd, flags) = kept(file);
                Sqe { opcode: OP_FSYNC, flags, fd, user_data, ..Sqe::default() }
            }
            Op::Write { fd, dir, bytes, put, names, to, .. } => {
                let fd = fd.as_raw_fd();
                sqes[1] =
                    Sqe { opcode: OP_FSYNC, flags: SQE_IO_LINK, fd, user_data: data(id, 1), ..Sqe::default() };
                sqes[2] = rename(names, *to, SQE_IO_LINK, data(id, 2));
                sqes[3] = Sqe { opcode: OP_FSYNC, fd: dir.as_raw_fd(), user_data: data(id, 3), ..Sqe::default() };
                sqes[0] = Sqe {
                    opcode: OP_WRITE,
                    flags: SQE_IO_LINK,
                    fd,
                    off: *put as u64,
                    addr: bytes[*put..].as_ptr() as u64,
                    len: (bytes.len() - put).min(u32::MAX as usize) as u32,
                    user_data,
                    ..Sqe::default()
                };
                return WRITE_LINKS;
            }
            Op::Rename { names, to, dirs, .. } => {
                let count = dirs.iter().flatten().count();
                for (n, dir) in dirs.iter().flatten().enumerate() {
                    let flags = if n + 1 < count { SQE_IO_LINK } else { 0 };
                    sqes[1 + n] = Sqe {
                        opcode: OP_FSYNC,
                        flags,
                        fd: dir.as_raw_fd(),
                        user_data: data(id, 1 + n),
                        ..Sqe::default()
                    };
                }
                sqes[0] = rename(names, *to, if count == 0 { 0 } else { SQE_IO_LINK }, user_data);
                return 1 + count;
            }
            Op::Watch { watch } => {
                let watching = self.watchings.get(*watch).expect("a watch's read's watching");
                Sqe {
                    opcode: OP_READ,
                    fd: watching.tree.raw(),
                    off: u64::MAX,
                    addr: watching.buffer.as_ptr() as u64,
                    len: watching.buffer.len() as u32,
                    user_data,
                    ..Sqe::default()
                }
            }
            Op::Cancel { target } => Sqe {
                opcode: OP_ASYNC_CANCEL,
                fd: -1,
                addr: *target,
                user_data,
                ..Sqe::default()
            },
            Op::Exit { pidfd, .. } => Sqe {
                opcode: OP_POLL_ADD,
                fd: *pidfd,
                op_flags: libc::POLLIN as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Output { fd, buffer, .. } => Sqe {
                opcode: OP_READ,
                fd: fd.as_raw_fd(),
                // A pipe has no position.
                off: u64::MAX,
                addr: buffer.as_ptr() as u64,
                len: buffer.len() as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Input { fd, bytes, put, .. } => Sqe {
                opcode: OP_WRITE,
                fd: fd.as_raw_fd(),
                off: u64::MAX,
                addr: bytes[*put..].as_ptr() as u64,
                len: (bytes.len() - put).min(u32::MAX as usize) as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Accept { listener, .. } => Sqe {
                opcode: OP_ACCEPT,
                ioprio: ACCEPT_MULTISHOT,
                fd: listener.fd(),
                op_flags: libc::SOCK_CLOEXEC as u32,
                // Into a slot of the kernel's half of the table, which the completion names.
                splice_fd_in: FILE_INDEX_ALLOC as i32,
                user_data,
                ..Sqe::default()
            },
            Op::Connect { socket, at, .. } => {
                let (fd, flags) = socket.as_ref().expect("a connect's socket").target();
                Sqe {
                    opcode: OP_CONNECT,
                    flags,
                    fd,
                    addr: &raw const at.storage as u64,
                    // The address's length.
                    off: u64::from(at.len),
                    user_data,
                    ..Sqe::default()
                }
            }
            Op::Recv { conn } => {
                let (fd, flags) = socket(conn);
                Sqe {
                    opcode: OP_RECV,
                    flags: flags | SQE_BUFFER_SELECT,
                    ioprio: RECV_MULTISHOT,
                    fd,
                    buf_index: RECV_GROUP,
                    user_data,
                    ..Sqe::default()
                }
            }
            Op::Send { conn, bytes, put } => {
                let (fd, flags) = socket(conn);
                Sqe {
                    opcode: OP_SEND,
                    flags,
                    fd,
                    addr: bytes[*put..].as_ptr() as u64,
                    len: (bytes.len() - put).min(u32::MAX as usize) as u32,
                    op_flags: libc::MSG_NOSIGNAL as u32,
                    user_data,
                    ..Sqe::default()
                }
            }
            Op::Shutdown { conn } => {
                let (fd, flags) = socket(conn);
                Sqe {
                    opcode: OP_SHUTDOWN,
                    flags,
                    fd,
                    len: libc::SHUT_RDWR as u32,
                    user_data,
                    ..Sqe::default()
                }
            }
            Op::Wake { fd, count } => Sqe {
                opcode: OP_READ,
                fd: *fd,
                off: u64::MAX,
                addr: count.get() as u64,
                len: 8,
                user_data,
                ..Sqe::default()
            },
            Op::Stepping => unreachable!("a request asked while it is stepped"),
        };
        1
    }

    /// Whether `WriteAt` of `len` bytes at `at` through `fd` bypasses the page cache, `fd`
    /// set to do so if it does: a write of [`DIRECT_LEAST`] or more whose offset and length
    /// are multiples of the alignment its file system says such writes need. A file system
    /// that refuses `O_DIRECT` (tmpfs) is remembered, and its writes go through the cache.
    fn direct(&mut self, fd: RawFd, at: u64, len: usize) -> io::Result<bool> {
        const SECTOR: u64 = 512;
        if len < DIRECT_LEAST || at % SECTOR != 0 || len as u64 % SECTOR != 0 {
            return Ok(false);
        }
        let (device, align) = direct_alignment(fd)?;
        let align = u64::from(align);
        if align == 0
            || self.refused.contains(&device)
            || DIRECT_PIECE as u64 % align != 0
            || at % align != 0
            || len as u64 % align != 0
        {
            return Ok(false);
        }
        // SAFETY: an open descriptor; no pointers.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: as above.
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_DIRECT) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                self.refused.insert(device);
                return Ok(false);
            }
            return Err(error);
        }
        Ok(true)
    }

    /// `effect` done by [`crate::threads::run`] on a thread of its own, its completion taken
    /// in at a poll once the thread wakes the ring.
    fn off_ring(&mut self, effect: Effect) -> io::Result<()> {
        let tag = effect.tag();
        self.off_ring_with(tag, move |root| crate::threads::run(root, effect))
    }

    /// `work`, given the root, done on a thread of its own for the effect `tag`, as
    /// [`Uring::off_ring`] does an effect.
    fn off_ring_with(
        &mut self,
        tag: Tag,
        work: impl FnOnce(&Path) -> io::Result<Done> + Send + 'static,
    ) -> io::Result<()> {
        let root = Arc::clone(&self.root);
        // clone: a channel's handle, for the thread to answer on.
        let sender = self.offload.sender.clone();
        let wake = Arc::clone(&self.offload.wake);
        std::thread::Builder::new().name("blaze-uring-off".into()).spawn(move || {
            // alloc: the error of work that failed.
            let done = work(&root).unwrap_or_else(|error| Done::Failed(IoError(error.to_string())));
            // A driver dropped meanwhile takes nothing.
            // The descriptor is kept open by this thread's `Arc`.
            if sender.send(Completion { tag, done }).is_ok() {
                wake_ring(&wake);
            }
        })?;
        self.offloaded(tag);
        Ok(())
    }

    /// Work for the effect `tag` was handed off the ring: the ring reads its wake until every
    /// such work's completion is in.
    fn offloaded(&mut self, tag: Tag) {
        self.offload.out += 1;
        if self.offload.reading.is_none() {
            let fd = self.offload.wake.as_raw_fd();
            let count = Rc::clone(&self.offload.count);
            let id = self.start(tag, Op::Wake { fd, count });
            self.offload.reading = Some(id);
        }
    }

    /// The [`Stampers`], started for the first sweep or listing.
    fn make_stampers(&mut self) -> io::Result<()> {
        if self.stampers.is_none() {
            self.stampers = Some(Stampers::new()?);
        }
        Ok(())
    }

    /// Starts the listing of [`Effect::Entries`] `tag` of `dir` on a stamper.
    fn list(&mut self, tag: Tag, dir: FileName) -> io::Result<()> {
        self.make_stampers()?;
        let list = List {
            tag,
            root: Arc::clone(&self.root),
            dir,
            // clone: a channel's handle, one per listing.
            sender: self.offload.sender.clone(),
            wake: Arc::clone(&self.offload.wake),
        };
        self.offloaded(tag);
        self.stampers.as_ref().expect("made above").list(list);
        Ok(())
    }

    /// Starts the sweep of [`Effect::Stamp`] `tag` over `names` on the [`Stampers`], made for
    /// the first. Fails only where the root cannot be opened or the stampers cannot start.
    fn sweep(&mut self, tag: Tag, names: Arc<Names>) -> io::Result<()> {
        let root = open_dir(&self.root)
            // alloc: the error of a sweep that failed.
            .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", self.root.display())))?;
        if names.is_empty() {
            // alloc: empty, nothing allocated.
            self.ready.push(Completion { tag, done: Done::Stamped(Vec::new()) });
            return Ok(());
        }
        self.make_stampers()?;
        // alloc: one per sweep, shared by the stampers.
        let job = Arc::new(Job::new(tag, root, names, &self.offload));
        self.offloaded(tag);
        self.stampers.as_ref().expect("made above").start(job);
        Ok(())
    }

    /// The completions the threads off the ring have sent.
    fn take_offloaded(&mut self) {
        while let Ok(completion) = self.offload.receiver.try_recv() {
            self.offload.out -= 1;
            self.ready.push(completion);
        }
    }

    fn flush_waiting(&mut self) {
        let mut sqes = [Sqe::default(); WRITE_LINKS];
        while let Some(&id) = self.waiting.front() {
            if !self.pending.contains(id) {
                self.waiting.pop_front();
                continue;
            }
            let n = self.sqes(id, &mut sqes);
            if !self.ring.push(&sqes[..n]) {
                break;
            }
            self.waiting.pop_front();
        }
    }

    fn submit_ring(&mut self) {
        if self.ring.unsubmitted > 0
            && let Err(error) = self.ring.enter(None)
        {
            self.fail_unsubmitted(error);
        }
    }

    /// The ring takes no more requests: every effect in flight fails. What the kernel may
    /// still hold of them stays in its slot, abandoned, never freed, and so do the watches,
    /// spawns' pipes and connections their buffers and files are.
    fn fail_unsubmitted(&mut self, error: io::Error) {
        // alloc: clone: the error path, once, when the ring dies.
        let why = format!("the ring failed: {error}");
        // clone: the error path, once per effect in flight.
        let failed = |tag| Completion { tag, done: Done::Failed(IoError(why.clone())) };
        for (_, pending) in self.pending.iter_mut().filter(|(_, it)| !it.abandoned) {
            pending.abandoned = true;
            match &mut pending.op {
                Op::Watch { watch } => {
                    let watching = self.watchings.get_mut(*watch).expect("a watch's read's watching");
                    self.ready.extend(watching.flushes.drain(..).map(failed));
                    self.ready.push(failed(pending.tag));
                }
                // A spawn fails once, below; work off the ring still completes through its
                // thread; a receive's effects fail with their connection's, below.
                Op::Cancel { .. }
                | Op::Exit { .. }
                | Op::Output { .. }
                | Op::Input { .. }
                | Op::Recv { .. }
                | Op::Wake { .. } => {}
                _ => self.ready.push(failed(pending.tag)),
            }
        }
        // Timers need no ring, but end with it as everything in flight does.
        self.timers.drain(|_, tag| self.ready.push(failed(tag)));
        // alloc: the error path, once.
        let mut spawns: Vec<(u64, Handle<Spawning>)> = self.spawning.drain().map(|(tag, it)| (tag.0, it)).collect();
        radix::sort_copy_by_key(&mut spawns, |it| it.0);
        for (_, spawn) in spawns {
            let mut spawn = self.spawns.remove(spawn).expect("in flight");
            if !spawn.exited {
                _ = kill(spawn.pidfd.as_raw_fd());
            }
            spawn.failed = Some(why.clone()); // clone: the error path.
            let tag = spawn.tag;
            self.ready.push(Completion { tag, done: spawn.end() });
        }
        for (_, conn) in self.conns.iter_mut() {
            for (tag, _) in conn.recvs.drain(..) {
                self.ready.push(failed(tag));
            }
            conn.receiving = None;
        }
        self.watches.clear();
        self.listens.clear();
        self.offload.reading = None;
        self.waiting.clear();
        self.for_buffers.clear();
        self.ring.unsubmitted = 0;
    }

    /// Takes the completion whose `user_data` is `user_data`: its effect's completion, or the
    /// rest of it asked for. With [`CQE_F_MORE`] in `flags` the kernel still holds the
    /// request (a multishot one), so it is kept and not asked again.
    ///
    /// A completion whose handle is stale (its request ended, its slot empty or another's since:
    /// a read cancelled after it completed, a request let go when the ring failed) is dropped,
    /// as is one of a request abandoned in place.
    fn completed(&mut self, user_data: u64, res: i32, flags: u32) {
        let (id, link) = of_data(user_data);
        let Some(pending) = self.pending.get_mut(id) else { return };
        if pending.abandoned {
            return;
        }
        let tag = pending.tag;
        let more = flags & CQE_F_MORE != 0;
        // Moves nothing the kernel may still read (see `Op`).
        let mut op = std::mem::replace(&mut pending.op, Op::Stepping);
        let step = self.step(tag, &mut op, link, res, flags);
        let again = |this: &mut Uring, op: Op| {
            this.pending.get_mut(id).expect("kept while stepped").op = op;
            if !more {
                this.queue(id);
            }
        };
        match step {
            Step::Again => again(self, op),
            Step::Goes(done) => {
                self.ready.push(Completion { tag, done });
                again(self, op);
            }
            Step::Held => self.pending.get_mut(id).expect("kept while stepped").op = op,
            Step::Ends(done) => {
                if let Some(done) = done {
                    self.ready.push(Completion { tag, done });
                }
                self.pending.remove(id);
                self.ended(tag, op);
            }
        }
    }

    /// What the result `res` of link `link` of a request of the effect `tag` means for it.
    fn step(&mut self, tag: Tag, op: &mut Op, link: usize, res: i32, flags: u32) -> Step {
        // alloc: called only for a request that failed.
        let error = |res: i32| io::Error::from_raw_os_error(-res).to_string();
        let failed = |res: i32| Some(Done::Failed(IoError(error(res))));
        let more = flags & CQE_F_MORE != 0;
        match op {
            Op::Cancel { .. } => Step::Ends(None),
            Op::Read { bytes, got, .. } => {
                if res < 0 {
                    return Step::Ends(failed(res));
                }
                *got += res as usize;
                if res > 0 && *got < bytes.len() {
                    return Step::Again;
                }
                let mut bytes = std::mem::take(bytes);
                bytes.truncate(*got);
                Step::Ends(Some(Done::Read(bytes)))
            }
            Op::Append { bytes, put, .. } => {
                if res < 0 {
                    return Step::Ends(failed(res));
                }
                *put += res as usize;
                match (*put < bytes.len(), res) {
                    (false, _) => Step::Ends(Some(Done::Appended)),
                    (true, 0) => Step::Ends(failed(-libc::EIO)),
                    (true, _) => Step::Again,
                }
            }
            Op::WriteAt { bytes, put, .. } => {
                if res < 0 {
                    return Step::Ends(failed(res));
                }
                *put += res as usize;
                match (*put < bytes.len(), res) {
                    (false, _) => Step::Ends(Some(Done::Written)),
                    (true, 0) => Step::Ends(failed(-libc::EIO)),
                    (true, _) => Step::Again,
                }
            }
            Op::Sync { .. } if res < 0 => Step::Ends(failed(res)),
            Op::Sync { .. } => Step::Ends(Some(Done::Synced)),
            Op::Write { bytes, put, names, file, chain, .. } => {
                let Some([wrote, synced, renamed, dir]) = chain.take(link, res, WRITE_LINKS) else {
                    return Step::Held;
                };
                // A short write breaks the chain; the rest is asked again, with the links
                // after it.
                if wrote > 0 && *put + (wrote as usize) < bytes.len() {
                    *put += wrote as usize;
                    return Step::Again;
                }
                self.forget(file);
                let wrote = if wrote == 0 && *put < bytes.len() { -libc::EIO } else { wrote };
                match [wrote, synced, renamed].into_iter().find(|it| *it < 0) {
                    // The temporary was never renamed: the old file is as it was.
                    Some(res) => {
                        // SAFETY: the temporary's path, NUL-terminated, at the start of `names`.
                        unsafe { libc::unlink(names.as_ptr().cast()) };
                        Step::Ends(failed(res))
                    }
                    None if dir < 0 => Step::Ends(failed(dir)),
                    None => Step::Ends(Some(Done::Written)),
                }
            }
            Op::Rename { dirs, chain, .. } => {
                let links = 1 + dirs.len();
                let Some(results) = chain.take(link, res, links) else { return Step::Held };
                match results[..links].iter().find(|it| **it < 0) {
                    Some(&res) => Step::Ends(failed(res)),
                    None => Step::Ends(Some(Done::Renamed)),
                }
            }
            Op::Watch { watch } => {
                let Uring { watchings, ready, watches, .. } = self;
                let Watching { tree, buffer, stopping, flushes, journal } =
                    watchings.get_mut(*watch).expect("a watch's read's watching");
                match res {
                    // Cancelled for a flush, or by a flush's cancel that came after the read it
                    // was for.
                    _ if res == -libc::ECANCELED && !*stopping => {
                        flushed(ready, tag, tree, buffer, flushes, journal.as_ref());
                        Step::Again
                    }
                    _ if res == -libc::ECANCELED || (res < 0 && *stopping) => {
                        watches.remove(&tag);
                        unflushed(ready, flushes);
                        Step::Ends(Some(Done::Unwatched))
                    }
                    1.. => {
                        let done = told(journal.as_ref(), tree.take(&buffer[..res as usize]));
                        ready.push(Completion { tag, done });
                        if !*stopping {
                            flushed(ready, tag, tree, buffer, flushes, journal.as_ref());
                            return Step::Again;
                        }
                        // The read was done before the cancel reached it: this change, then the
                        // end.
                        watches.remove(&tag);
                        unflushed(ready, flushes);
                        Step::Ends(Some(Done::Unwatched))
                    }
                    _ => {
                        watches.remove(&tag);
                        unflushed(ready, flushes);
                        Step::Ends(failed(if res < 0 { res } else { -libc::EIO }))
                    }
                }
            }
            Op::Exit { spawn, .. } => {
                if res < 0 {
                    self.spoil(*spawn, format!("waiting for its exit: {}", error(res))); // alloc: an error.
                } else if let Some(spawn) = self.spawns.get_mut(*spawn) {
                    spawn.exited = true;
                }
                Step::Ends(None)
            }
            Op::Output { spawn, buffer, stderr, .. } => {
                if res > 0 {
                    if let Some(spawn) = self.spawns.get_mut(*spawn) {
                        let to = if *stderr { &mut spawn.stderr } else { &mut spawn.stdout };
                        // alloc: the spawn's own output, grown as it comes.
                        to.extend_from_slice(&buffer[..res as usize]);
                    }
                    return Step::Again;
                }
                if res < 0 {
                    let pipe = if *stderr { "stderr" } else { "stdout" };
                    self.spoil(*spawn, format!("reading its {pipe}: {}", error(res))); // alloc: an error.
                }
                Step::Ends(None)
            }
            Op::Input { spawn, bytes, put, .. } => {
                // It ended, or closed its input, before reading all of it.
                if res == -libc::EPIPE {
                    return Step::Ends(None);
                }
                if res < 0 || (res == 0 && *put < bytes.len()) {
                    let why = if res < 0 { error(res) } else { error(-libc::EIO) };
                    self.spoil(*spawn, format!("writing its input: {why}")); // alloc: an error.
                    return Step::Ends(None);
                }
                *put += res as usize;
                match *put < bytes.len() {
                    true => Step::Again,
                    false => Step::Ends(None),
                }
            }
            Op::Accept { stopping, .. } => {
                if res >= 0 {
                    let socket = self.table.accepted(res as u32);
                    let conn = self.add(socket);
                    let done = Done::Accepted { conn };
                    // Stopping, it ends only at the completion that says the kernel let go of
                    // it: until then it may accept more.
                    if !*stopping || more {
                        return Step::Goes(done);
                    }
                    self.listens.remove(&tag);
                    self.ready.push(Completion { tag, done });
                    return Step::Ends(Some(Done::Unlistened));
                }
                if more {
                    // The kernel goes on accepting with it: it has not ended.
                    return Step::Held;
                }
                if res == -libc::ECANCELED || *stopping {
                    self.listens.remove(&tag);
                    return Step::Ends(Some(Done::Unlistened));
                }
                if res == -libc::ENFILE {
                    self.listens.remove(&tag);
                    // alloc: an error.
                    let why = format!(
                        "{}: every one of the {} slots the ring's table of files keeps for \
                         accepted connections is in use",
                        error(res),
                        self.table.ours
                    );
                    return Step::Ends(Some(Done::Failed(IoError(why))));
                }
                // A connection that went before it was taken, or the network's errors that
                // accept(2) says to take as "try again".
                let again = [
                    libc::ECONNABORTED,
                    libc::EINTR,
                    libc::EPROTO,
                    libc::ENETDOWN,
                    libc::ENOPROTOOPT,
                    libc::EHOSTDOWN,
                    libc::ENONET,
                    libc::EHOSTUNREACH,
                    libc::EOPNOTSUPP,
                    libc::ENETUNREACH,
                ];
                if again.contains(&-res) {
                    return Step::Again;
                }
                self.listens.remove(&tag);
                Step::Ends(failed(res))
            }
            Op::Connect { socket, .. } => {
                if res < 0 {
                    return Step::Ends(failed(res));
                }
                let socket = socket.take().expect("a connect's socket");
                let conn = self.add(socket);
                Step::Ends(Some(Done::Connected { conn }))
            }
            Op::Recv { conn } => {
                let conn = *conn;
                if flags & CQE_F_BUFFER != 0 {
                    let bid = (flags >> CQE_BUFFER_SHIFT) as u16;
                    if res > 0
                        && let Some(connection) = self.conns.get_mut(conn_of(conn))
                        && !connection.hung
                    {
                        connection.inbox.extend(self.provided.bytes(bid, res as usize));
                    }
                    self.provided.give(bid);
                }
                let Some(connection) = self.conns.get_mut(conn_of(conn)) else {
                    return if more { Step::Held } else { Step::Ends(None) };
                };
                if !more {
                    connection.receiving = None;
                    connection.pausing = false;
                    match res {
                        // The peer hung up, or reset it: as good as hung up, for a reader.
                        0 => connection.ended = Some(Ok(())),
                        _ if res == -libc::ECONNRESET => connection.ended = Some(Ok(())),
                        // Out of buffers, or paused: armed again once it is wanted.
                        _ if res == -libc::ENOBUFS || res == -libc::ECANCELED || res > 0 => {}
                        _ => connection.ended = Some(Err(error(res))),
                    }
                    return Step::Ends(None);
                }
                let pause = connection.inbox.len() >= INBOX && !connection.pausing;
                if pause {
                    connection.pausing = true;
                    let id = connection.receiving.expect("an armed receive");
                    self.start(NOBODYS, Op::Cancel { target: data(id, 0) });
                }
                self.deliver(conn);
                Step::Held
            }
            Op::Wake { .. } => {
                self.take_offloaded();
                if res >= 0 && self.offload.out > 0 {
                    return Step::Again;
                }
                // Nothing more is off the ring, or the read failed: what is still out is taken
                // in at the polls that follow, and the next work off the ring reads again.
                self.offload.reading = None;
                Step::Ends(None)
            }
            Op::Send { bytes, put, .. } => {
                if res < 0 {
                    return Step::Ends(failed(res));
                }
                *put += res as usize;
                match (*put < bytes.len(), res) {
                    (false, _) => Step::Ends(Some(Done::Sent)),
                    (true, 0) => Step::Ends(failed(-libc::EIO)),
                    (true, _) => Step::Again,
                }
            }
            Op::Shutdown { .. } if res < 0 && res != -libc::ENOTCONN => Step::Ends(failed(res)),
            Op::Shutdown { .. } => Step::Ends(Some(Done::HungUp)),
        }
    }

    fn reap(&mut self) {
        self.take_offloaded();
        let mut posted = std::mem::take(&mut self.posted);
        self.ring.reap(|user_data, res, flags| posted.push((user_data, res, flags)));
        for (user_data, res, flags) in posted.drain(..) {
            self.completed(user_data, res, flags);
        }
        self.posted = posted;
        self.flush_waiting();
        self.submit_ring();
    }

    /// Stops the watch or listen whose request is `id`: through the ring, or here if the
    /// request has not reached the kernel yet.
    fn stop(&mut self, id: Id) {
        let Some(pending) = self.pending.get_mut(id) else { return };
        let stopping = match &mut pending.op {
            Op::Watch { watch } => &mut self.watchings.get_mut(*watch).expect("a watch's read's watching").stopping,
            Op::Accept { stopping, .. } => stopping,
            _ => unreachable!("a watch's or a listen's request"),
        };
        if std::mem::replace(stopping, true) {
            return;
        }
        let tag = pending.tag;
        let Some(at) = self.waiting.iter().position(|&it| it == id) else {
            self.start(tag, Op::Cancel { target: data(id, 0) });
            return;
        };
        self.waiting.remove(at);
        let pending = self.pending.remove(id).expect("in flight");
        let done = match &pending.op {
            Op::Watch { watch } => {
                self.watches.remove(&tag);
                let watching = self.watchings.get_mut(*watch).expect("a watch's read's watching");
                unflushed(&mut self.ready, &mut watching.flushes);
                Done::Unwatched
            }
            _ => {
                self.listens.remove(&tag);
                Done::Unlistened
            }
        };
        self.ready.push(Completion { tag, done });
        self.ended(tag, pending.op);
    }

    /// Asks the watch `watch` for the flush `tag`. Its read, run by the ring's kernel thread,
    /// would race a read of the descriptor here, so a read in the kernel is cancelled and the
    /// flush answered once its completion is in ([`flushed`]); a read still waiting for room
    /// in the ring reads nothing, so the descriptor is drained now.
    fn flush(&mut self, watch: Tag, tag: Tag) -> io::Result<()> {
        let Some(&id) = self.watches.get(&watch) else {
            // alloc: an error path.
            return Err(io::Error::other(format!("nothing is watched as {watch:?}")));
        };
        let waiting = self.waiting.contains(&id);
        let Some(Pending { op: Op::Watch { watch: watching }, .. }) = self.pending.get(id) else {
            unreachable!("a watch's request")
        };
        let Watching { tree, buffer, stopping, flushes, journal } =
            self.watchings.get_mut(*watching).expect("a watch's read's watching");
        if *stopping {
            return Err(io::Error::other("the watch ended before the flush"));
        }
        if waiting {
            flushes.push(tag);
            flushed(&mut self.ready, watch, tree, buffer, flushes, journal.as_ref());
            return Ok(());
        }
        flushes.push(tag);
        if flushes.len() == 1 {
            self.start(watch, Op::Cancel { target: data(id, 0) });
        }
        Ok(())
    }

    /// An effect that fails as it is submitted, with no request of its own to end.
    fn refuse(&mut self, tag: Tag, why: String) {
        self.in_flight += 1;
        self.ready.push(Completion { tag, done: Done::Failed(IoError(why)) });
    }

    fn spawn(&mut self, tag: Tag, command: Box<Command>) -> io::Result<()> {
        let crate::process::Started { pid, pidfd, stdin, stdout, stderr, mut finish } =
            crate::process::start(&self.root, command, self.confine.as_ref())?;
        let input = std::mem::take(&mut finish.command.stdin);
        let exit = pidfd.as_raw_fd();
        let parts = 3 + usize::from(stdin.is_some());
        let spawn = self.spawns.insert(Spawning {
            tag,
            pid,
            pidfd,
            finish,
            stdout: Vec::new(), // alloc: empty until the process writes; the spawn's own output.
            stderr: Vec::new(), // alloc: empty until the process writes; the spawn's own output.
            parts,
            exited: false,
            failed: None,
        });
        self.spawning.insert(tag, spawn);
        self.start(tag, Op::Exit { spawn, pidfd: exit });
        let buffer = self.buffer();
        self.start(tag, Op::Output { spawn, fd: stdout, buffer, stderr: false });
        let buffer = self.buffer();
        self.start(tag, Op::Output { spawn, fd: stderr, buffer, stderr: true });
        if let Some(fd) = stdin {
            self.start(tag, Op::Input { spawn, fd, bytes: input, put: 0 });
        }
        Ok(())
    }

    /// A request of `spawn` ended; once all have, it completes.
    fn part_ended(&mut self, spawn: Handle<Spawning>) {
        let Some(it) = self.spawns.get_mut(spawn) else { return };
        it.parts -= 1;
        if it.parts == 0 {
            let it = self.spawns.remove(spawn).expect("in flight");
            self.spawning.remove(&it.tag);
            let tag = it.tag;
            self.ready.push(Completion { tag, done: it.end() });
        }
    }

    /// `spawn` completes failed, saying `why`; its process is killed so that it ends.
    fn spoil(&mut self, spawn: Handle<Spawning>, why: String) {
        let Some(spawn) = self.spawns.get_mut(spawn) else { return };
        spawn.failed.get_or_insert(why);
        if !spawn.exited {
            _ = kill(spawn.pidfd.as_raw_fd());
        }
    }

    fn kill(&mut self, tag: Tag) {
        let Some(&handle) = self.spawning.get(&tag) else { return };
        let spawn = self.spawns.get(handle).expect("a spawn in flight");
        if spawn.exited {
            return;
        }
        if let Err(error) = kill(spawn.pidfd.as_raw_fd()) {
            self.spoil(handle, format!("killing it: {error}")); // alloc: an error.
        }
    }

    fn add(&mut self, socket: Fixed) -> Conn {
        let conn = self.conns.insert(Connection {
            socket,
            users: 0,
            hung: false,
            inbox: VecDeque::new(), // alloc: empty until it receives; grown once and reused.
            recvs: VecDeque::new(), // alloc: empty until it is asked; grown once and reused.
            receiving: None,
            pausing: false,
            ended: None,
        });
        Conn(conn.to_bits())
    }

    /// `conn`, used by one more request until [`Uring::release`] says it ended.
    fn use_conn(&mut self, conn: Conn) -> io::Result<()> {
        let connection = self.conns.get_mut(conn_of(conn)).filter(|it| !it.hung).ok_or_else(|| gone(conn))?;
        connection.users += 1;
        Ok(())
    }

    fn release(&mut self, conn: Conn) {
        let Some(connection) = self.conns.get_mut(conn_of(conn)) else { return };
        connection.users -= 1;
        if connection.hung && connection.users == 0 {
            self.conns.remove(conn_of(conn));
        }
    }

    /// A receive of at most `most` bytes on `conn`: from what its receive took, in order,
    /// once there is any.
    fn recv(&mut self, conn: Conn, tag: Tag, most: usize) -> io::Result<()> {
        let connection = self.conns.get_mut(conn_of(conn)).filter(|it| !it.hung).ok_or_else(|| gone(conn))?;
        connection.recvs.push_back((tag, most));
        self.deliver(conn);
        Ok(())
    }

    /// Hands `conn`'s waiting `Recv`s what its inbox holds, or its end, and arms its receive
    /// where one waits for bytes not yet taken.
    fn deliver(&mut self, conn: Conn) {
        let Some(connection) = self.conns.get_mut(conn_of(conn)) else { return };
        while let Some(&(tag, most)) = connection.recvs.front() {
            let done = match &connection.ended {
                _ if most == 0 => Done::Read(Vec::new()), // alloc: empty, nothing allocated.
                _ if !connection.inbox.is_empty() => {
                    let n = most.min(connection.inbox.len());
                    let (front, back) = connection.inbox.as_slices();
                    let split = front.len().min(n);
                    let mut bytes = Vec::with_capacity(n);
                    bytes.extend_from_slice(&front[..split]);
                    bytes.extend_from_slice(&back[..n - split]);
                    connection.inbox.drain(..n);
                    Done::Read(bytes)
                }
                Some(Ok(())) => Done::Read(Vec::new()), // alloc: empty, nothing allocated.
                Some(Err(why)) => Done::Failed(IoError(why.clone())), // clone: an error.
                None => break,
            };
            connection.recvs.pop_front();
            self.ready.push(Completion { tag, done });
        }
        let arm = connection.receiving.is_none()
            && connection.ended.is_none()
            && !connection.hung
            && !connection.recvs.is_empty()
            && connection.inbox.len() < INBOX;
        if !arm {
            return;
        }
        connection.users += 1;
        let id = self.start(NOBODYS, Op::Recv { conn });
        if let Some(connection) = self.conns.get_mut(conn_of(conn)) {
            connection.receiving = Some(id);
        }
    }

    /// Shuts `conn` both ways through the ring, its waiting receives answered empty, and lets
    /// it go once nothing in flight uses it.
    fn hang_up(&mut self, conn: Conn, tag: Tag) -> io::Result<()> {
        let connection = self.conns.get_mut(conn_of(conn)).filter(|it| !it.hung).ok_or_else(|| gone(conn))?;
        connection.hung = true;
        connection.inbox.clear();
        connection.users += 1;
        let receiving = connection.receiving;
        for (tag, _) in connection.recvs.drain(..) {
            self.ready.push(Completion { tag, done: Done::Read(Vec::new()) }); // alloc: empty.
        }
        if let Some(id) = receiving {
            self.start(NOBODYS, Op::Cancel { target: data(id, 0) });
        }
        self.start(tag, Op::Shutdown { conn });
        Ok(())
    }

    /// Starts [`Effect::Write`] `tag` of `bytes` over `file`: its temporary made next to it,
    /// and the chain that writes, syncs and renames it over the file asked.
    fn write(&mut self, tag: Tag, file: FileName, bytes: Vec<u8>) -> io::Result<()> {
        let mut names = self.path();
        let made = under(&self.root, &file, |path| {
            crate::fs::make_parent(path)?;
            push_temporary(&mut names, path)?;
            let to = push_c(&mut names, path)?;
            let dir = open_dir_of(path)?;
            let temporary = CStr::from_bytes_with_nul(&names[..to]).expect("NUL-terminated as pushed");
            Ok::<_, io::Error>((to, dir, create(temporary)?))
        });
        let (to, dir, fd) = match made {
            Ok(it) => it,
            Err(error) => {
                self.give_back_path(names);
                return Err(error);
            }
        };
        self.start(tag, Op::Write { fd, dir, bytes, put: 0, names, to, file, chain: Chain::default() });
        Ok(())
    }

    /// Starts [`Effect::Rename`] `tag` of `from` over `to`, linked to the syncs of the
    /// directory of each.
    fn rename(&mut self, tag: Tag, from: &FileName, to: &FileName) -> io::Result<()> {
        self.forget(from);
        self.forget(to);
        let mut names = self.path();
        let made = under(&self.root, from, |from| {
            under(&self.root, to, |to| {
                crate::fs::make_parent(to)?;
                push_c(&mut names, from)?;
                let at = push_c(&mut names, to)?;
                let mut dirs = [Some(open_dir_of(to)?), None];
                if from.parent() != to.parent() {
                    dirs[1] = Some(open_dir_of(from)?);
                }
                Ok::<_, io::Error>((at, dirs))
            })
        });
        let (at, dirs) = match made {
            Ok(it) => it,
            Err(error) => {
                self.give_back_path(names);
                return Err(error);
            }
        };
        self.start(tag, Op::Rename { names, to: at, dirs, chain: Chain::default() });
        Ok(())
    }

    /// A buffer of names kept for the next write or rename, where there is room.
    fn give_back_path(&mut self, mut names: Vec<u8>) {
        if self.paths.len() < SPARES {
            names.clear();
            self.paths.push(names);
        }
    }

    /// Starts the watch `tag` of `dir`, into the tables of one that ended where there is one;
    /// an [`Effect::Journal`]'s with `journal`.
    fn watch(&mut self, tag: Tag, dir: FileName, journal: bool) -> io::Result<()> {
        let (spare, flushes) = match self.trees.pop() {
            Some((spare, flushes)) => (Some(spare), flushes),
            None => (None, Vec::new()), // alloc: empty until a flush; kept for the next watch.
        };
        let tree = crate::watch::Tree::reusing(spare, &self.root, &dir, true)?;
        let buffer = self.buffer();
        let journal = journal.then_some(dir);
        let watch = self.watchings.insert(Watching { tree, buffer, stopping: false, flushes, journal });
        let id = self.start(tag, Op::Watch { watch });
        self.watches.insert(tag, id);
        Ok(())
    }

    /// Writes that bypassed the page cache (`O_DIRECT`) and ended, since the driver was made.
    pub fn direct_writes(&self) -> u64 {
        self.direct_writes
    }
}

/// The rename of the first of `names` over the second, which starts at `to`, each
/// NUL-terminated and relative to the working directory.
fn rename(names: &[u8], to: usize, flags: u8, user_data: u64) -> Sqe {
    debug_assert!(to > 0 && names[to - 1] == 0 && names.last() == Some(&0));
    Sqe {
        opcode: OP_RENAMEAT,
        flags,
        fd: libc::AT_FDCWD,
        addr: names.as_ptr() as u64,
        // The new name's directory, and the new name.
        len: libc::AT_FDCWD as u32,
        off: names[to..].as_ptr() as u64,
        user_data,
        ..Sqe::default()
    }
}

impl Drop for Uring {
    /// Kills the processes of spawns in flight and takes back from the kernel every buffer
    /// it was lent (every request cancelled, its completions waited for), so none is freed
    /// while the kernel may still write into it. What it does not give back in time is leaked,
    /// with the provided buffers its receives write into.
    fn drop(&mut self) {
        for (_, spawn) in self.spawns.iter() {
            if !spawn.exited {
                _ = kill(spawn.pidfd.as_raw_fd());
            }
        }
        // Never handed to the kernel.
        for id in std::mem::take(&mut self.waiting).into_iter().chain(std::mem::take(&mut self.for_buffers)) {
            self.pending.remove(id);
        }
        let mut owed: FxHashMap<Id, usize> =
            self.pending.iter().map(|(id, it)| (id, it.op.owed())).collect(); // alloc: drop, once.
        let deadline = Instant::now() + LETTING_GO;
        let mut posted = Vec::new(); // alloc: the driver's drop, once.
        let cancel = Sqe {
            opcode: OP_ASYNC_CANCEL,
            fd: -1,
            op_flags: ASYNC_CANCEL_ALL | ASYNC_CANCEL_ANY,
            user_data: NOBODY,
            ..Sqe::default()
        };
        let mut alive = true;
        while alive && !self.pending.is_empty() && !self.ring.push(&[cancel]) {
            alive = Instant::now() < deadline && self.ring.enter(None).is_ok();
            self.ring.reap(|user_data, res, flags| posted.push((user_data, res, flags)));
        }
        while alive {
            for (user_data, res, flags) in posted.drain(..) {
                let (id, _) = of_data(user_data);
                match self.pending.get(id).map(|it| &it.op) {
                    // A connection the accept made after it was cancelled, let go here.
                    Some(Op::Accept { .. }) if res >= 0 => _ = self.table.set(res as u32, -1),
                    Some(Op::Recv { .. }) if flags & CQE_F_BUFFER != 0 => {
                        self.provided.give((flags >> CQE_BUFFER_SHIFT) as u16);
                    }
                    _ => {}
                }
                // A multishot request the kernel still holds is not given back yet.
                if flags & CQE_F_MORE == 0
                    && let Some(left) = owed.get_mut(&id)
                {
                    *left -= 1;
                    if *left == 0 {
                        // A connection's or a spawn's request let go: what it used goes with
                        // the driver below.
                        self.pending.remove(id);
                    }
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let lent = self.pending.iter().any(|(_, it)| !matches!(it.op, Op::Cancel { .. }));
            if !lent || left.is_zero() {
                break;
            }
            alive = self.ring.enter(Some(left)).is_ok();
            self.ring.reap(|user_data, res, flags| posted.push((user_data, res, flags)));
        }
        self.spawning.clear();
        // alloc: the driver's drop, once.
        let spawns: Vec<Handle<Spawning>> = self.spawns.iter().map(|(it, _)| it).collect();
        for spawn in spawns {
            _ = self.spawns.remove(spawn).expect("listed").end();
        }
        let lent = self.pending.iter().any(|(_, it)| !matches!(it.op, Op::Cancel { .. }));
        if !lent {
            // SAFETY: each dropped once, here; no request the kernel holds reads or writes into
            // what they hold. Where one does, they are leaked with what it uses.
            unsafe {
                ManuallyDrop::drop(&mut self.provided);
                ManuallyDrop::drop(&mut self.pending);
                ManuallyDrop::drop(&mut self.watchings);
            }
        }
    }
}

/// SIGKILL to the process of `pidfd`; one that has exited already is left as it is.
fn kill(pidfd: RawFd) -> io::Result<()> {
    // SAFETY: no pointers but a null `siginfo`; a pidfd names its process even once reaped,
    // so the signal never reaches another that took its pid.
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            libc::SIGKILL,
            ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if sent == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        _ => Err(error),
    }
}

fn gone(conn: Conn) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        format!("{conn:?}: no such connection (never made, or hung up)"), // alloc: an error.
    )
}

/// Answers the flushes of the watch `watch`, whose read has completed and is not yet asked
/// again, so nothing else reads its descriptor: what is still queued on it is told first.
fn flushed(
    ready: &mut Vec<Completion>,
    watch: Tag,
    tree: &mut crate::watch::Tree,
    buffer: &mut [u8],
    flushes: &mut Vec<Tag>,
    journal: Option<&FileName>,
) {
    if flushes.is_empty() {
        return;
    }
    let drained =
        tree.drain(buffer, |done| ready.push(Completion { tag: watch, done: told(journal, done) }));
    for tag in flushes.drain(..) {
        let done = match &drained {
            Ok(()) => Done::Flushed,
            // alloc: an error path.
            Err(error) => Done::Failed(IoError(format!("flushing the watch: {error}"))),
        };
        ready.push(Completion { tag, done });
    }
}

/// What a watch's read says, as its effect tells it: for a journal, as the batch of a journal
/// on a system that keeps none.
fn told(journal: Option<&FileName>, done: Done) -> Done {
    match journal {
        Some(dir) => crate::journal::live(dir, done),
        None => done,
    }
}

/// Fails the flushes of a watch that ended first.
fn unflushed(ready: &mut Vec<Completion>, flushes: &mut Vec<Tag>) {
    for tag in flushes.drain(..) {
        let why = IoError("the watch ended before the flush".into());
        ready.push(Completion { tag, done: Done::Failed(why) });
    }
}

/// Forgets the descriptor kept for `name`: the name now is another file. It is closed once no
/// request in flight holds it.
fn forget(names: &mut FxHashMap<FileName, Handle<Kept>>, kept: &mut Slab<Kept>, name: &FileName) {
    let Some(file) = names.remove(name) else { return };
    let it = kept.get_mut(file).expect("named files are kept");
    it.named = false;
    if it.users == 0 {
        kept.remove(file);
    }
}

/// A request that held `file` ended.
fn unkeep(kept: &mut Slab<Kept>, file: Handle<Kept>) {
    let Some(it) = kept.get_mut(file) else { return };
    it.users -= 1;
    if it.users == 0 && !it.named {
        kept.remove(file);
    }
}

/// `f` of `name` under `root`, joined in the thread's scratch path.
fn under<R>(root: &Path, name: &FileName, f: impl FnOnce(&Path) -> R) -> R {
    scratch::with_path(|path| {
        path.push(root);
        path.push(name.as_str());
        f(path)
    })
}

/// `f` of `path` NUL-terminated, in the thread's scratch bytes.
fn with_c<R>(path: &Path, f: impl FnOnce(&CStr) -> io::Result<R>) -> io::Result<R> {
    scratch::with_bytes(|bytes| {
        bytes.extend_from_slice(path.as_os_str().as_bytes());
        bytes.push(0);
        let c = CStr::from_bytes_with_nul(&bytes[..])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
        f(c)
    })
}

/// `path` NUL-terminated at the end of `into`; where it starts.
fn push_c(into: &mut Vec<u8>, path: &Path) -> io::Result<usize> {
    let at = into.len();
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"));
    }
    into.extend_from_slice(bytes);
    into.push(0);
    Ok(at)
}

/// A name next to `path`'s, at the end of `into` NUL-terminated, that no other write of this
/// process or another uses, as [`crate::fs::temporary_next_to`] makes one.
fn push_temporary(into: &mut Vec<u8>, path: &Path) -> io::Result<usize> {
    let at = into.len();
    if let Some(parent) = path.parent() {
        into.extend_from_slice(parent.as_os_str().as_bytes());
        if !parent.as_os_str().is_empty() {
            into.push(b'/');
        }
    }
    let leaf = path.file_name().map_or(&[][..], |it| it.as_bytes());
    let n = TEMPORARY.fetch_add(1, Ordering::Relaxed);
    into.push(b'.');
    into.extend_from_slice(leaf);
    write!(into, ".{}-{n}.tmp", std::process::id())?;
    if into[at..].contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"));
    }
    into.push(0);
    Ok(at)
}

fn open(path: &Path, make: bool) -> io::Result<OwnedFd> {
    let flags =
        libc::O_RDWR | libc::O_APPEND | libc::O_CLOEXEC | if make { libc::O_CREAT } else { 0 };
    with_c(path, |path| open_c(path, flags))
}

/// `path` opened with `flags`, made `0o644` where they make it.
fn open_c(path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: a NUL-terminated path; the descriptor returned is this driver's.
    let fd = unsafe { libc::open(path.as_ptr(), flags, 0o644 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A file made for writing, that was not there.
fn create(path: &CStr) -> io::Result<OwnedFd> {
    open_c(path, libc::O_WRONLY | libc::O_CLOEXEC | libc::O_CREAT | libc::O_EXCL)
}

/// A file opened for writing where it is told to; `flags` added to the opening's.
fn open_to_write(path: &Path, flags: libc::c_int) -> io::Result<OwnedFd> {
    with_c(path, |path| open_c(path, libc::O_WRONLY | libc::O_CLOEXEC | flags))
}

/// The directory `path` is in, opened to be synced.
fn open_dir_of(path: &Path) -> io::Result<OwnedFd> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    open_dir(dir)
}

/// The directory `dir`, opened to be synced or for calls relative to it.
fn open_dir(dir: &Path) -> io::Result<OwnedFd> {
    with_c(dir, |dir| open_c(dir, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC))
}

fn size_of_file(fd: RawFd) -> io::Result<u64> {
    // SAFETY: `libc::stat` is a C struct of integers and arrays of them, for which all-zero
    // bytes is a valid value.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: an open descriptor and a `stat` to write.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.st_size as u64)
}

/// Makes a TCP listener's connections, which they inherit it from, send each write at once.
fn no_delay(listener: &crate::net::Listener) -> io::Result<()> {
    if listener.is_unix() {
        return Ok(());
    }
    let on: libc::c_int = 1;
    // SAFETY: an open socket and an int option of the size given.
    let set = unsafe {
        libc::setsockopt(
            listener.fd(),
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            (&raw const on).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if set != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A socket made for a connect, its writes sent at once where it is TCP's, in the driver's
/// half of the table.
fn connect_socket(table: &Rc<Table>, domain: libc::c_int) -> io::Result<Fixed> {
    let fd = crate::net::stream_socket(domain)?;
    if domain != libc::AF_UNIX {
        let on: libc::c_int = 1;
        // SAFETY: an open socket and an int option of the size given.
        let set = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_NODELAY,
                (&raw const on).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if set != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    table.install(fd)
}

impl Driver for Uring {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            let tag = effect.tag();
            match effect {
                Effect::Unwatch { .. } => {
                    match self.watches.get(&tag).copied() {
                        Some(id) => self.stop(id),
                        None => self.refuse(tag, format!("nothing is watched as {tag:?}")), // alloc: an error.
                    }
                    continue;
                }
                Effect::Unlisten { .. } => {
                    match self.listens.get(&tag).copied() {
                        Some(id) => self.stop(id),
                        None => self.refuse(tag, format!("nothing is listened on as {tag:?}")), // alloc: an error.
                    }
                    continue;
                }
                Effect::Kill { .. } => {
                    self.kill(tag);
                    continue;
                }
                _ => {}
            }
            self.in_flight += 1;
            let started = match effect {
                Effect::Write { file, bytes, .. } => self.write(tag, file, bytes).map(|()| None),
                Effect::Clone { from, to, .. } => {
                    self.forget(&to);
                    under(&self.root, &from, |from| under(&self.root, &to, |to| crate::fs::clone(from, to)))
                        .map(|()| Some(Done::Cloned))
                }
                Effect::Link { from, to, .. } => {
                    self.forget(&to);
                    under(&self.root, &from, |from| under(&self.root, &to, |to| crate::fs::link(from, to)))
                        .map(|()| Some(Done::Linked))
                }
                Effect::Stat { file, .. } => {
                    under(&self.root, &file, crate::fs::stat).map(|it| Some(Done::Stat(it)))
                }
                list @ Effect::List { .. } => self.off_ring(list).map(|()| None),
                Effect::WriteAt { file, at: offset, bytes, .. } => {
                    under(&self.root, &file, |path| open_to_write(path, 0)).and_then(|fd| match bytes.is_empty() {
                        true => Ok(Some(Done::Written)),
                        false => {
                            let direct = self.direct(fd.as_raw_fd(), offset, bytes.len())?;
                            let op = Op::WriteAt { fd, at: offset, bytes, put: 0, direct, buffer: None };
                            self.start(tag, op);
                            Ok(None)
                        }
                    })
                }
                Effect::Rename { from, to, .. } => self.rename(tag, &from, &to).map(|()| None),
                Effect::Mode { file, mode, .. } => {
                    under(&self.root, &file, |path| crate::fs::set_mode(path, mode)).map(|()| Some(Done::Moded))
                }
                // No io_uring op sets a file's times: one `utimensat` here on the driver's
                // thread, as a mode is set, its path in the thread's scratch buffers. A thread
                // of its own (`off_ring`) would cost a spawn per file for a call of microseconds.
                Effect::Times { file, modified_ns, .. } => {
                    under(&self.root, &file, |path| crate::fs::set_times(path, modified_ns)).map(|()| Some(Done::Timed))
                }
                Effect::Remove { file, .. } => {
                    self.forget(&file);
                    under(&self.root, &file, crate::fs::remove).map(|()| Some(Done::Removed))
                }
                Effect::Watch { dir, .. } => self.watch(tag, dir, false).map(|()| None),
                // Linux keeps no journal across a restart: the first batch says what of the
                // cursor is lost, and the watch is live from its start.
                Effect::Journal { dir, since, .. } => {
                    let first = crate::journal::first_on_linux(&dir, &since);
                    self.watch(tag, dir, true).map(|()| {
                        self.ready.push(Completion { tag, done: first });
                        None
                    })
                }
                Effect::Stamp { files, .. } => self.sweep(tag, files).map(|()| None),
                Effect::Entries { dir, .. } => self.list(tag, dir).map(|()| None),
                Effect::Flush { watch, .. } => self.flush(watch, tag).map(|()| None),
                Effect::Unwatch { .. } | Effect::Unlisten { .. } | Effect::Kill { .. } => {
                    unreachable!("taken above")
                }
                Effect::ReadAll { file, .. } => match self.file(&file, false) {
                    Ok(file) => size_of_file(self.kept.get(file).expect("just kept").fixed.fd()).map(|size| {
                        // alloc: the read's result, handed to its asker.
                        let bytes = vec![0; size as usize];
                        match size {
                            0 => Some(Done::Read(bytes)),
                            _ => {
                                let file = self.hold(file);
                                self.start(tag, Op::Read { file, at: 0, bytes, got: 0 });
                                None
                            }
                        }
                    }),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        Ok(Some(Done::Read(Vec::new()))) // alloc: empty, nothing allocated.
                    }
                    Err(error) => Err(error),
                },
                Effect::Read { file, at, len, .. } => self.file(&file, false).map(|file| {
                    let file = self.hold(file);
                    // alloc: the read's result, handed to its asker.
                    self.start(tag, Op::Read { file, at, bytes: vec![0; len], got: 0 });
                    None
                }),
                Effect::Append { file, bytes, .. } => {
                    self.file(&file, true).map(|file| match bytes.is_empty() {
                        true => Some(Done::Appended),
                        false => {
                            let file = self.hold(file);
                            self.start(tag, Op::Append { file, bytes, put: 0 });
                            None
                        }
                    })
                }
                Effect::Sync { file, .. } => self.file(&file, true).map(|file| {
                    let file = self.hold(file);
                    self.start(tag, Op::Sync { file });
                    None
                }),
                Effect::Truncate { file, len, .. } => self.file(&file, true).and_then(|file| {
                    let fd = self.kept.get(file).expect("just kept").fixed.fd();
                    // SAFETY: an open descriptor.
                    match unsafe { libc::ftruncate(fd, len as libc::off_t) } {
                        0 => Ok(Some(Done::Truncated)),
                        _ => Err(io::Error::last_os_error()),
                    }
                }),
                Effect::Timer { after, .. } => {
                    let due = wheel::ticks(self.start.elapsed()).saturating_add(wheel::ticks(after));
                    self.timers.insert(due, tag);
                    Ok(None)
                }
                Effect::Map { file, .. } => under(&self.root, &file, crate::fs::map).map(|it| Some(Done::Mapped(it))),
                Effect::Spawn { command, .. } => self.spawn(tag, command).map(|()| None),
                Effect::Listen { at: address, .. } => {
                    crate::net::listen(&self.root, &address).and_then(|listener| {
                        no_delay(&listener)?;
                        let id = self.start(tag, Op::Accept { listener, stopping: false });
                        self.listens.insert(tag, id);
                        Ok(None)
                    })
                }
                Effect::Connect { at: address, .. } => crate::net::sockaddr(&self.root, &address)
                    .and_then(|address| {
                        let socket = connect_socket(&self.table, address.domain)?;
                        let op = Op::Connect { socket: Some(socket), at: address };
                        self.start(tag, op);
                        Ok(None)
                    }),
                Effect::Recv { conn, most, .. } => self.recv(conn, tag, most).map(|()| None),
                Effect::Send { conn, bytes, .. } if bytes.is_empty() => self
                    .conns
                    .get(conn_of(conn))
                    .filter(|it| !it.hung)
                    .map(|_| Some(Done::Sent))
                    .ok_or_else(|| gone(conn)),
                Effect::Send { conn, bytes, .. } => self.use_conn(conn).map(|()| {
                    self.start(tag, Op::Send { conn, bytes, put: 0 });
                    None
                }),
                Effect::Hangup { conn, .. } => self.hang_up(conn, tag).map(|()| None),
            };
            match started {
                Ok(None) => {}
                Ok(Some(done)) => self.ready.push(Completion { tag, done }),
                Err(error) => self
                    .ready
                    .push(Completion { tag, done: Done::Failed(IoError(error.to_string())) }), // alloc: an error.
            }
        }
        self.flush_waiting();
        self.submit_ring();
    }

    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize {
        let before = out.len();
        self.reap();
        self.fire_timers();
        out.append(&mut self.ready);
        if out.len() == before && self.in_flight > 0 {
            let deadline = Instant::now() + wait;
            while out.len() == before {
                let left = deadline.saturating_duration_since(Instant::now());
                let now = wheel::ticks(self.start.elapsed());
                let until_timer =
                    self.timers.next_deadline().map(|due| Duration::from_nanos(due.saturating_sub(now)));
                if let Err(error) = self.ring.enter(Some(until_timer.map_or(left, |it| it.min(left)))) {
                    self.fail_unsubmitted(error);
                    out.append(&mut self.ready);
                    break;
                }
                self.reap();
                self.fire_timers();
                out.append(&mut self.ready);
                if left.is_zero() {
                    break;
                }
            }
        }
        // A watch or a journal goes on after a change, and a listen after an accept.
        let ended = out[before..]
            .iter()
            .filter(|it| !matches!(it.done, Done::Changed { .. } | Done::Journaled(_) | Done::Accepted { .. }))
            .count();
        self.in_flight -= ended;
        out.len() - before
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        Address, Confined, Cursor, FileName, Journaled, Lost, Sandbox, Traced, Untraceable,
        run_until_idle, threads::Threads,
    };

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The same effects on the ring and on threads end the same.
    #[test]
    fn as_threads_do() {
        let file = FileName::new("vfs/journal").unwrap();
        fn script(driver: &mut impl Driver, file: &FileName) -> Vec<Done> {
            let mut effects =
                vec![Effect::Append { file: file.clone(), bytes: b"one".to_vec(), tag: Tag(1) }];
            let mut seen = Vec::new();
            run_until_idle(driver, &mut effects, |done, effects| {
                seen.push(done.done.clone());
                match done.tag {
                    Tag(1) => effects.push(Effect::Sync { file: file.clone(), tag: Tag(2) }),
                    Tag(2) => {
                        effects.push(Effect::Timer { after: Duration::from_millis(1), tag: Tag(3) })
                    }
                    Tag(3) => effects.push(Effect::Read {
                        file: file.clone(),
                        at: 1,
                        len: 10,
                        tag: Tag(4),
                    }),
                    Tag(4) => {
                        effects.push(Effect::Truncate { file: file.clone(), len: 2, tag: Tag(5) })
                    }
                    Tag(5) => effects.push(Effect::ReadAll { file: file.clone(), tag: Tag(6) }),
                    Tag(6) => effects.push(Effect::ReadAll {
                        file: FileName::new("missing").unwrap(),
                        tag: Tag(7),
                    }),
                    Tag(7) => effects.push(Effect::Read {
                        file: FileName::new("missing").unwrap(),
                        at: 0,
                        len: 1,
                        tag: Tag(8),
                    }),
                    _ => {}
                }
            });
            seen
        }
        let (ours, theirs) = (dir("uring"), dir("uring-threads"));
        let ring = script(&mut Uring::new(&ours).unwrap(), &file);
        let threads = script(&mut Threads::new(&theirs, 2), &file);
        assert_eq!(&ring[..7], &threads[..7]);
        assert_eq!(
            &ring[..7],
            &[
                Done::Appended,
                Done::Synced,
                Done::Fired,
                Done::Read(b"ne".to_vec()),
                Done::Truncated,
                Done::Read(b"on".to_vec()),
                Done::Read(Vec::new()),
            ]
        );
        assert!(matches!((&ring[7], &threads[7]), (Done::Failed(_), Done::Failed(_))));
        for it in [ours, theirs] {
            std::fs::remove_dir_all(it).unwrap();
        }
    }

    /// More requests than the ring holds, all in flight at once, each to its own file, with
    /// timers among them that fire in their order.
    #[test]
    fn more_than_the_ring_holds() {
        let root = dir("uring-many");
        let mut driver = Uring::new(&root).unwrap();
        let count = ENTRIES as u64 * 4;
        let mut effects: Vec<Effect> = (0..count)
            .map(|n| Effect::Append {
                file: FileName::new(&format!("f/{}", n % 64)).unwrap(),
                bytes: n.to_le_bytes().to_vec(),
                tag: Tag(n),
            })
            .collect();
        effects.extend((0..3).map(|n| Effect::Timer {
            after: Duration::from_millis(5 * (3 - n)),
            tag: Tag(count + n),
        }));
        let mut appended = 0;
        let mut fired = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| match done.done {
            Done::Appended => appended += 1,
            Done::Fired => fired.push(done.tag.0 - count),
            other => panic!("{other:?}"),
        });
        assert_eq!((appended, fired), (count, vec![2, 1, 0]));
        let mut total = 0;
        for n in 0..64 {
            total += std::fs::read(root.join(format!("f/{n}"))).unwrap().len();
        }
        assert_eq!(total as u64, count * 8);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_wait_ends_at_its_time() {
        let mut driver = Uring::new(dir("uring-wait")).unwrap();
        let mut effects = vec![Effect::Timer { after: Duration::from_secs(60), tag: Tag(1) }];
        driver.submit(&mut effects);
        let began = Instant::now();
        let mut out = Vec::new();
        assert_eq!(driver.poll(&mut out, Duration::from_millis(20)), 0);
        let waited = began.elapsed();
        assert!(
            waited >= Duration::from_millis(19) && waited < Duration::from_secs(5),
            "{waited:?}"
        );
        assert_eq!(driver.in_flight(), 1);
    }

    fn command(program: &str, args: &[&str], stdin: &[u8]) -> Command {
        Command {
            program: program.into(),
            args: args.iter().map(Into::into).collect(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            dir: "/".into(),
            stdin: stdin.to_vec(),
            sandbox: Sandbox::Off,
            memory: None,
        }
    }

    fn sh(script: &str, stdin: &[u8]) -> Box<Command> {
        Box::new(command("/bin/sh", &["-c", script], stdin))
    }

    /// A completion as two drivers must agree on it: without the memory measured, which
    /// differs from run to run, and a failure without its words.
    fn shown(done: &Done) -> String {
        let text = |bytes: &[u8]| match bytes.len() {
            0..100 => format!("{:?}", String::from_utf8_lossy(bytes)),
            len => format!("{len} bytes"),
        };
        match done {
            Done::Exited(exit) => format!(
                "exited code {:?} signal {:?} stdout {} stderr {} traced {:?}",
                exit.code,
                exit.signal,
                text(&exit.stdout),
                text(&exit.stderr),
                exit.traced
            ),
            Done::Mapped(mapping) => format!("mapped {}", text(mapping.bytes())),
            Done::Failed(_) => "failed".into(),
            other => format!("{other:?}"),
        }
    }

    /// Processes with input, output, an exit code and a signal; one killed; one in a sandbox
    /// no driver was given the means to make; one whose program is not there.
    fn processes(driver: &mut impl Driver) -> BTreeMap<u64, Done> {
        let unconfined = Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: false };
        let mut effects = vec![
            Effect::Spawn { command: sh("tr a-z A-Z; echo oops >&2; exit 3", b"hello\n"), tag: Tag(1) },
            Effect::Spawn { command: sh("kill -TERM $$", b""), tag: Tag(2) },
            Effect::Spawn { command: sh("exec sleep 60", b""), tag: Tag(3) },
            Effect::Spawn {
                command: sh("wc -c; head -c 200000 /dev/zero", &vec![b'x'; 300_000]),
                tag: Tag(4),
            },
            Effect::Spawn {
                command: Box::new(Command { sandbox: unconfined, ..command("/bin/true", &[], b"") }),
                tag: Tag(5),
            },
            Effect::Spawn {
                command: Box::new(command("/nonexistent/blaze-test", &[], b"")),
                tag: Tag(6),
            },
            // A kill of nothing in flight does nothing.
            Effect::Kill { tag: Tag(77) },
            Effect::Timer { after: Duration::from_millis(50), tag: Tag(7) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(driver, &mut effects, |done, effects| {
            if done.tag == Tag(7) {
                effects.push(Effect::Kill { tag: Tag(3) });
            }
            seen.insert(done.tag.0, done.done);
        });
        assert_eq!(driver.in_flight(), 0);
        seen
    }

    #[test]
    fn processes_end_as_on_threads() {
        let ring = processes(&mut Uring::new(dir("uring-spawn")).unwrap());
        let exited = |tag: u64| match &ring[&tag] {
            Done::Exited(exit) => exit.clone(),
            other => panic!("{tag}: {other:?}"),
        };
        let one = exited(1);
        assert_eq!((one.code, one.signal), (Some(3), None));
        assert_eq!((&one.stdout[..], &one.stderr[..]), (&b"HELLO\n"[..], &b"oops\n"[..]));
        assert_eq!((exited(2).code, exited(2).signal), (None, Some(libc::SIGTERM)));
        assert_eq!(exited(3).signal, Some(libc::SIGKILL));
        let four = exited(4);
        assert_eq!(four.code, Some(0));
        assert!(four.stdout.starts_with(b"300000\n"), "{:?}", four.stdout.get(..10));
        assert_eq!(four.stdout.len(), 7 + 200_000);
        assert!(matches!(&ring[&5], Done::Failed(why) if why.0.contains("no Confine")), "{:?}", ring[&5]);
        assert!(
            matches!(&ring[&6], Done::Failed(why) if why.0.contains("/nonexistent/blaze-test")),
            "{:?}",
            ring[&6]
        );
        assert_eq!(ring[&7], Done::Fired);
        assert!(!ring.contains_key(&77));
        let threads = processes(&mut Threads::new(dir("uring-spawn-threads"), 2));
        let show = |it: &BTreeMap<u64, Done>| -> BTreeMap<u64, String> {
            it.iter().map(|(tag, done)| (*tag, shown(done))).collect()
        };
        assert_eq!(show(&ring), show(&threads));
    }

    struct Fake;

    fn fake_trace() -> Traced {
        Traced {
            files: vec![("/etc/hostname".into(), Some([7; 32])), ("/missing".into(), None)],
            dirs: vec![("/usr".into(), [1; 32])],
            env: vec![("HOME".into(), None)],
            untraceable: vec![Untraceable::Clock],
        }
    }

    impl Confine for Fake {
        fn confine(&self, command: &Command, scratch: &Path) -> Result<Confined, String> {
            assert!(scratch.is_dir(), "{}", scratch.display());
            Ok(Confined {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), format!("echo confined {}", command.args.len()).into()],
                env: Vec::new(),
                dir: "/".into(),
            })
        }

        fn traced(&self, _: &Command, scratch: &Path) -> Result<Traced, String> {
            assert!(scratch.is_dir(), "{}", scratch.display());
            Ok(fake_trace())
        }
    }

    /// A sandboxed spawn runs what its `Confine` says, and its trace is the sandbox's.
    #[test]
    fn a_confined_spawn_is_traced() {
        let root = dir("uring-confined");
        let mut driver = Uring::new(&root).unwrap().confined(Arc::new(Fake));
        let sandbox =
            Sandbox::On { read: vec!["/usr".into()], write: Vec::new(), network: false, trace: true };
        let mut effects = vec![Effect::Spawn {
            command: Box::new(Command { sandbox, ..command("/usr/bin/env", &["a", "b"], b"") }),
            tag: Tag(1),
        }];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
        let [Done::Exited(exit)] = &seen[..] else { panic!("{seen:?}") };
        assert_eq!(exit.stdout, b"confined 2\n");
        assert_eq!(exit.traced, Some(fake_trace()));
        let left: Vec<_> = std::fs::read_dir(root.join(".blaze-spawn")).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A connection to the listen `Tag(1)`, submitted already, at `at`: a ping and a pong, each
    /// side hung up, a receive on a connection gone, the listen ended and an unlisten of
    /// nothing. What each step did, by its tag, with connections by their role.
    fn round_trip(driver: &mut impl Driver, at: Address) -> BTreeMap<u64, Vec<String>> {
        let mut seen: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        let (mut server, mut client, mut pinged) = (None, None, false);
        let mut effects = vec![Effect::Timer { after: Duration::from_millis(20), tag: Tag(2) }];
        run_until_idle(driver, &mut effects, |done, effects| {
            let tag = done.tag.0;
            let shown = match &done.done {
                Done::Accepted { conn } => {
                    server = Some(*conn);
                    "accepted".to_owned()
                }
                Done::Connected { conn } => {
                    client = Some(*conn);
                    "connected".to_owned()
                }
                other => shown(other),
            };
            seen.entry(tag).or_default().push(shown);
            if tag == 2 {
                effects.push(Effect::Connect { at: at.clone(), tag: Tag(3) });
            }
            let (Some(server), Some(client)) = (server, client) else { return };
            assert_ne!(server, client);
            if !pinged {
                pinged = true;
                effects.push(Effect::Send { conn: client, bytes: b"ping".to_vec(), tag: Tag(4) });
                effects.push(Effect::Recv { conn: server, most: 64, tag: Tag(5) });
            }
            match tag {
                5 => {
                    effects.push(Effect::Send { conn: server, bytes: b"pong".to_vec(), tag: Tag(6) });
                    effects.push(Effect::Recv { conn: client, most: 64, tag: Tag(7) });
                }
                7 => effects.push(Effect::Hangup { conn: client, tag: Tag(8) }),
                8 => {
                    effects.push(Effect::Recv { conn: server, most: 64, tag: Tag(9) });
                    effects.push(Effect::Recv { conn: client, most: 64, tag: Tag(10) });
                }
                9 => {
                    effects.push(Effect::Hangup { conn: server, tag: Tag(11) });
                    effects.push(Effect::Unlisten { tag: Tag(1) });
                    effects.push(Effect::Unlisten { tag: Tag(12) });
                }
                _ => {}
            }
        });
        assert_eq!(driver.in_flight(), 0);
        seen
    }

    fn round_tripped() -> BTreeMap<u64, Vec<String>> {
        let one = |it: &str| vec![it.to_owned()];
        let read = |it: &[u8]| one(&format!("{:?}", Done::Read(it.to_vec())));
        BTreeMap::from([
            (1, vec!["accepted".to_owned(), "Unlistened".to_owned()]),
            (2, one("Fired")),
            (3, one("connected")),
            (4, one("Sent")),
            (5, read(b"ping")),
            (6, one("Sent")),
            (7, read(b"pong")),
            (8, one("HungUp")),
            (9, read(b"")),
            (10, one("failed")),
            (11, one("HungUp")),
            (12, one("failed")),
        ])
    }

    #[test]
    fn a_unix_socket_round_trip_as_on_threads() {
        let at = Address::Unix(FileName::new("run/api.sock").unwrap());
        let root = dir("uring-unix");
        let mut ring = Uring::new(&root).unwrap();
        ring.submit(&mut vec![Effect::Listen { at: at.clone(), tag: Tag(1) }]);
        assert_eq!(ring.in_flight(), 1);
        assert_eq!(ring.local_addr(Tag(1)), None);
        let ours = round_trip(&mut ring, at.clone());
        assert_eq!(ours, round_tripped());
        assert!(!root.join("run/api.sock").exists());
        let mut threads = Threads::new(dir("uring-unix-threads"), 2);
        threads.submit(&mut vec![Effect::Listen { at: at.clone(), tag: Tag(1) }]);
        assert_eq!(ours, round_trip(&mut threads, at));
    }

    #[test]
    fn a_tcp_round_trip() {
        let mut ring = Uring::new(dir("uring-tcp")).unwrap();
        let any = Address::Tcp("127.0.0.1:0".parse().unwrap());
        ring.submit(&mut vec![Effect::Listen { at: any, tag: Tag(1) }]);
        assert_eq!(ring.in_flight(), 1);
        let at = ring.local_addr(Tag(1)).expect("listening");
        assert_ne!(at.port(), 0);
        assert_eq!(round_trip(&mut ring, Address::Tcp(at)), round_tripped());
        assert_eq!(ring.local_addr(Tag(1)), None);
        // Nothing listens there now.
        let mut effects = vec![Effect::Connect { at: Address::Tcp(at), tag: Tag(1) }];
        let mut seen = Vec::new();
        run_until_idle(&mut ring, &mut effects, |done, _| seen.push(done.done));
        assert!(matches!(&seen[..], [Done::Failed(_)]), "{seen:?}");
    }

    fn maps(driver: &mut impl Driver, root: &Path) -> BTreeMap<u64, String> {
        std::fs::create_dir_all(root.join("store")).unwrap();
        std::fs::write(root.join("store/pack"), b"packed").unwrap();
        std::fs::write(root.join("store/empty"), b"").unwrap();
        let name = |it: &str| FileName::new(it).unwrap();
        let mut effects = vec![
            Effect::Map { file: name("store/pack"), tag: Tag(1) },
            Effect::Map { file: name("store/empty"), tag: Tag(2) },
            Effect::Map { file: name("store/missing"), tag: Tag(3) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(driver, &mut effects, |done, _| _ = seen.insert(done.tag.0, shown(&done.done)));
        seen
    }

    #[test]
    fn a_mapping_is_the_file_as_on_threads() {
        let root = dir("uring-map");
        let ours = maps(&mut Uring::new(&root).unwrap(), &root);
        assert_eq!(
            ours,
            BTreeMap::from([
                (1, "mapped \"packed\"".to_owned()),
                (2, "mapped \"\"".to_owned()),
                (3, "failed".to_owned()),
            ])
        );
        let root = dir("uring-map-threads");
        assert_eq!(ours, maps(&mut Threads::new(&root, 2), &root));
    }

    /// Listings done off the ring: many at once, one of a missing directory; each wakes the
    /// poll that waits, with a timer far off in flight beside them.
    #[test]
    fn listings_wake_the_ring_from_their_threads() {
        let root = dir("uring-list");
        for file in ["tree/a", "tree/b/c", "tree/b/d/e"] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, file).unwrap();
        }
        let mut driver = Uring::new(&root).unwrap();
        let name = |it: &str| FileName::new(it).unwrap();
        let mut effects: Vec<Effect> =
            (0..16).map(|n| Effect::List { dir: name("tree"), tag: Tag(n) }).collect();
        effects.push(Effect::List { dir: name("missing"), tag: Tag(16) });
        effects.push(Effect::Timer { after: Duration::from_secs(60), tag: Tag(17) });
        driver.submit(&mut effects);
        let started = Instant::now();
        let mut seen = BTreeMap::new();
        let mut done = Vec::new();
        while seen.len() < 17 && started.elapsed() < Duration::from_secs(20) {
            driver.poll(&mut done, Duration::from_secs(10));
            for completion in done.drain(..) {
                let shown = match completion.done {
                    Done::Listed(found) => {
                        found.iter().map(|(it, _)| it.as_str()).collect::<Vec<_>>().join(" ")
                    }
                    Done::Failed(_) => "failed".to_owned(),
                    other => panic!("{other:?}"),
                };
                seen.insert(completion.tag.0, shown);
            }
        }
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        let all = "tree/a tree/b tree/b/c tree/b/d tree/b/d/e";
        let mut expected: BTreeMap<u64, String> = (0..16).map(|n| (n, all.to_owned())).collect();
        expected.insert(16, "failed".to_owned());
        assert_eq!(seen, expected);
        assert_eq!(driver.in_flight(), 1);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Dropping the driver with a process, a listen and a receive in flight kills the process
    /// and takes everything back from the kernel.
    #[test]
    fn a_driver_dropped_with_work_in_flight() {
        let root = dir("uring-drop");
        let mut ring = Uring::new(&root).unwrap();
        let at = Address::Unix(FileName::new("api.sock").unwrap());
        ring.submit(&mut vec![
            Effect::Spawn { command: sh("exec sleep 60", b""), tag: Tag(1) },
            Effect::Listen { at: at.clone(), tag: Tag(2) },
            Effect::Connect { at, tag: Tag(3) },
            Effect::Timer { after: Duration::from_secs(60), tag: Tag(4) },
        ]);
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client = None;
        while client.is_none() && Instant::now() < deadline {
            ring.poll(&mut out, Duration::from_millis(50));
            client = out.iter().find_map(|it| match it.done {
                Done::Connected { conn } => Some(conn),
                _ => None,
            });
        }
        let client = client.expect("connected");
        ring.submit(&mut vec![Effect::Recv { conn: client, most: 16, tag: Tag(5) }]);
        let pid = ring.spawns.get(ring.spawning[&Tag(1)]).expect("in flight").pid;
        let began = Instant::now();
        drop(ring);
        assert!(began.elapsed() < LETTING_GO, "{:?}", began.elapsed());
        // Reaped: no process of this pid is a child of this one any more.
        // SAFETY: no pointers.
        assert_eq!(unsafe { libc::waitpid(pid, ptr::null_mut(), libc::WNOHANG) }, -1);
        assert!(!root.join("api.sock").exists());
    }

    /// A file kept open is in the driver's half of the table, and its reads and appends name
    /// the slot; forgetting it empties the slot for the next.
    #[test]
    fn kept_files_are_registered() {
        let root = dir("uring-fixed");
        let mut driver = Uring::new(&root).unwrap();
        let name = |it: &str| FileName::new(it).unwrap();
        let mut effects = vec![
            Effect::Append { file: name("a"), bytes: b"one".to_vec(), tag: Tag(1) },
            Effect::Append { file: name("b"), bytes: b"two".to_vec(), tag: Tag(2) },
        ];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
        assert_eq!(seen, [Done::Appended, Done::Appended]);
        let slot = |driver: &Uring, it: &str| driver.kept.get(driver.names[&name(it)]).expect("kept").fixed.slot;
        let (a, b) = (slot(&driver, "a").expect("a slot"), slot(&driver, "b").expect("a slot"));
        assert!(a != b && a < driver.table.ours && b < driver.table.ours, "{a} {b}");
        let free = driver.table.free.borrow().len();
        let mut effects = vec![
            Effect::Read { file: name("a"), at: 0, len: 3, tag: Tag(3) },
            Effect::Remove { file: name("b"), tag: Tag(4) },
        ];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
        assert_eq!(seen, [Done::Read(b"one".to_vec()), Done::Removed]);
        assert_eq!(driver.table.free.borrow().len(), free + 1);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The kernel's submission thread sleeps once idle; a request written after is still
    /// taken, the thread woken by the enter that says so.
    #[test]
    fn a_sleeping_submission_thread_is_woken() {
        let root = dir("uring-sqpoll");
        let mut driver = Uring::new(&root).unwrap();
        let file = FileName::new("log").unwrap();
        for n in 0..3u8 {
            std::thread::sleep(Duration::from_millis(u64::from(SQ_THREAD_IDLE_MS) * 4));
            assert_ne!(driver.ring.flags() & SQ_NEED_WAKEUP, 0, "asleep after {SQ_THREAD_IDLE_MS} ms idle");
            let began = Instant::now();
            let mut effects = vec![Effect::Append { file: file.clone(), bytes: vec![n], tag: Tag(n.into()) }];
            let mut seen = Vec::new();
            run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
            assert_eq!(seen, [Done::Appended]);
            assert!(began.elapsed() < Duration::from_secs(1), "{:?}", began.elapsed());
        }
        assert_eq!(std::fs::read(root.join("log")).unwrap(), [0, 1, 2]);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Aligned writes of a MiB or more, more at once than there are registered buffers, each
    /// read back as written; where the file system takes `O_DIRECT` they bypassed the cache.
    /// A write that is not aligned goes through the cache.
    #[test]
    fn aligned_writes_read_back_equal() {
        let root = dir("uring-direct");
        std::fs::create_dir_all(&root).unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let pack = FileName::new("store/pack").unwrap();
        let piece = 3 * DIRECT_PIECE / 2;
        let count = 2 * DIRECT_BUFFERS as usize;
        let bytes = |n: usize| -> Vec<u8> { (0..piece).map(|at| (at * 31 + n * 7) as u8).collect() };
        std::fs::create_dir_all(root.join("store")).unwrap();
        std::fs::write(root.join("store/pack"), b"").unwrap();
        let mut effects: Vec<Effect> = (0..count)
            .map(|n| Effect::WriteAt { file: pack.clone(), at: (n * piece) as u64, bytes: bytes(n), tag: Tag(n as u64) })
            .collect();
        let odd = (count * piece) as u64 + 7;
        effects.push(Effect::WriteAt { file: pack.clone(), at: odd, bytes: vec![9; DIRECT_PIECE + 1], tag: Tag(99) });
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
        assert_eq!(seen, vec![Done::Written; count + 1]);
        let file = std::fs::read(root.join("store/pack")).unwrap();
        for n in 0..count {
            assert!(file[n * piece..(n + 1) * piece] == bytes(n), "piece {n}");
        }
        assert!(file[odd as usize..].iter().all(|&it| it == 9));
        let fd = open(&root.join("store/pack"), false).unwrap();
        let (device, align) = direct_alignment(fd.as_raw_fd()).unwrap();
        let aligned = align != 0 && piece % align as usize == 0 && DIRECT_PIECE % align as usize == 0;
        let direct = if aligned && !driver.refused.contains(&device) { count as u64 } else { 0 };
        assert_eq!(driver.direct_writes(), direct, "alignment {align}");
        assert_eq!(driver.registered.free.len(), DIRECT_BUFFERS as usize);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Many sends, one after another, received by one multishot receive through a ring of
    /// fewer buffers than they fill, in order and whole.
    #[test]
    fn one_receive_takes_many_messages_through_few_buffers() {
        let root = dir("uring-multishot");
        let mut ring = Uring::new(&root).unwrap();
        let at = Address::Unix(FileName::new("s.sock").unwrap());
        let message = |n: usize| -> Vec<u8> { (0..4096).map(|at| (at + n) as u8).collect() };
        let messages = 4 * RECV_BUFFERS as usize * RECV_BUFFER / 4096;
        let mut effects = vec![
            Effect::Listen { at: at.clone(), tag: Tag(1) },
            Effect::Connect { at, tag: Tag(2) },
        ];
        let (mut server, mut client) = (None, None);
        let (mut sent, mut got) = (0, Vec::new());
        let mut tag = 10;
        run_until_idle(&mut ring, &mut effects, |done, effects| {
            let mut next = || {
                tag += 1;
                Tag(tag)
            };
            match &done.done {
                Done::Accepted { conn } => server = Some(*conn),
                Done::Connected { conn } => client = Some(*conn),
                Done::Sent => sent += 1,
                Done::Read(bytes) => {
                    assert!(!bytes.is_empty(), "ended after {} bytes", got.len());
                    got.extend_from_slice(bytes);
                }
                Done::Unlistened | Done::HungUp => {}
                other => panic!("{other:?}"),
            }
            let (Some(server), Some(client)) = (server, client) else { return };
            if matches!(done.done, Done::Accepted { .. } | Done::Connected { .. }) {
                effects.push(Effect::Recv { conn: server, most: 64 << 10, tag: next() });
                effects.push(Effect::Send { conn: client, bytes: message(0), tag: next() });
                return;
            }
            if matches!(done.done, Done::Sent) && sent < messages {
                effects.push(Effect::Send { conn: client, bytes: message(sent), tag: next() });
            }
            if matches!(done.done, Done::Read(_)) {
                if got.len() < messages * 4096 {
                    effects.push(Effect::Recv { conn: server, most: 64 << 10, tag: next() });
                } else {
                    effects.push(Effect::Hangup { conn: client, tag: next() });
                    effects.push(Effect::Hangup { conn: server, tag: next() });
                    effects.push(Effect::Unlisten { tag: Tag(1) });
                }
            }
        });
        assert_eq!(sent, messages);
        let wanted: Vec<u8> = (0..messages).flat_map(message).collect();
        assert!(got == wanted, "{} bytes of {}", got.len(), wanted.len());
        assert_eq!(ring.in_flight(), 0);
        assert!(ring.conns.is_empty());
        drop(ring);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A whole file's write is one linked chain: written, synced, renamed over the old and the
    /// directory synced. Where the rename fails the old is as it was and no temporary is left;
    /// a rename across directories is linked to both directories' syncs.
    #[test]
    fn a_linked_write_whose_rename_fails_leaves_the_old() {
        let root = dir("uring-linked");
        std::fs::create_dir_all(root.join("out/blocked")).unwrap();
        std::fs::write(root.join("out/blocked/kept"), b"old").unwrap();
        std::fs::write(root.join("out/file"), b"old").unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let name = |it: &str| FileName::new(it).unwrap();
        let mut effects = vec![
            Effect::Write { file: name("out/blocked"), bytes: b"new".to_vec(), tag: Tag(1) },
            Effect::Write { file: name("out/file"), bytes: vec![5; 3 << 20], tag: Tag(2) },
            Effect::Write { file: name("out/fresh"), bytes: Vec::new(), tag: Tag(3) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(&mut driver, &mut effects, |done, _| _ = seen.insert(done.tag.0, shown(&done.done)));
        assert_eq!(
            seen,
            BTreeMap::from([(1, "failed".to_owned()), (2, "Written".to_owned()), (3, "Written".to_owned())])
        );
        assert_eq!(std::fs::read(root.join("out/blocked/kept")).unwrap(), b"old");
        assert_eq!(std::fs::read(root.join("out/file")).unwrap(), vec![5; 3 << 20]);
        assert_eq!(std::fs::read(root.join("out/fresh")).unwrap(), b"");
        let mut left: Vec<String> = std::fs::read_dir(root.join("out"))
            .unwrap()
            .map(|it| it.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["blocked", "file", "fresh"]);
        let mut effects = vec![
            Effect::Rename { from: name("out/file"), to: name("moved/file"), tag: Tag(4) },
            Effect::Rename { from: name("out/missing"), to: name("out/other"), tag: Tag(5) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(&mut driver, &mut effects, |done, _| _ = seen.insert(done.tag.0, shown(&done.done)));
        assert_eq!(seen, BTreeMap::from([(4, "Renamed".to_owned()), (5, "failed".to_owned())]));
        assert_eq!(std::fs::read(root.join("moved/file")).unwrap().len(), 3 << 20);
        assert!(!root.join("out/file").exists());
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// `files` files of 8 bytes in directories of 100 under `src`, one of them executable; a
    /// file at the top; a symlink to a file, one to nothing and one to a directory; a directory
    /// that can be searched and not read, with a file in it. The names to stamp of it: every
    /// file (one through the directory's link), directories, the links, and names with nothing
    /// there (under a file, under a dangling link, through the directory's link). Undone by
    /// [`unsweep`].
    fn sweep_tree(root: &Path, files: usize) -> Arc<Names> {
        let mut names = Names::default();
        for n in 0..files {
            if n % 100 == 0 {
                std::fs::create_dir_all(root.join(format!("src/d{}", n / 100))).unwrap();
            }
            let name = format!("src/d{}/f{n}.rs", n / 100);
            std::fs::write(root.join(&name), (n as u64).to_le_bytes()).unwrap();
            assert!(names.push_str(&name));
        }
        std::os::unix::fs::symlink("d0/f0.rs", root.join("src/link")).unwrap();
        std::os::unix::fs::symlink("nowhere", root.join("src/dangling")).unwrap();
        std::os::unix::fs::symlink("d0", root.join("src/via")).unwrap();
        crate::fs::set_mode(&root.join("src/d0/f1.rs"), 0o755).unwrap();
        std::fs::write(root.join("top.rs"), b"top").unwrap();
        std::fs::create_dir_all(root.join("src/sealed")).unwrap();
        std::fs::write(root.join("src/sealed/inside.rs"), b"in").unwrap();
        crate::fs::set_mode(&root.join("src/sealed"), 0o111).unwrap();
        let others = ["src", "src/d0", "src/link", "src/dangling", "src/via", "src/via/f0.rs", "top.rs", "src/sealed", "src/sealed/inside.rs"];
        for name in GONE.iter().chain(&others) {
            assert!(names.push_str(name));
        }
        Arc::new(names)
    }

    /// Files of [`sweep_tree`]'s besides the `files` in `src/d*`.
    const OTHER_FILES: usize = 3;

    /// Removes what [`sweep_tree`] made.
    fn unsweep(root: &Path) {
        crate::fs::set_mode(&root.join("src/sealed"), 0o755).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    const GONE: [&str; 5] =
        ["src/missing", "src/d0/f0.rs/under-a-file", "gone/deep/name", "src/dangling/name", "src/via/missing"];

    /// The stampers' sweep, over many chunks and directories, gives name by name the stamps of
    /// the reference sweep's `fstatat`s of whole names: files, directories, links not followed
    /// (but followed as a directory on the way), the mode's bits, a directory searched and not
    /// read, and names with nothing there gone.
    #[test]
    fn a_sweep_stamps_as_the_reference_does() {
        let root = dir("uring-stamp");
        let names = sweep_tree(&root, 10_000);
        assert!(names.len() > 8 * STAMP_CHUNK);
        let reference = crate::fs::stamps(&root, &names).unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let mut effects = vec![Effect::Stamp { files: names.clone(), tag: Tag(1) }];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done));
        let [Completion { tag: Tag(1), done: Done::Stamped(stamps) }] = &seen[..] else { panic!("{seen:?}") };
        assert_eq!(stamps.len(), names.len());
        for (at, (ours, theirs)) in stamps.iter().zip(&reference).enumerate() {
            assert_eq!(ours, theirs, "{}", names.get(at));
        }
        let of = |name: &str| stamps[names.iter().position(|it| it == name).expect("stamped")];
        assert_eq!(stamps.iter().filter(|it| it.is_file()).count(), 10_000 + OTHER_FILES);
        assert!(stamps.iter().all(|it| !it.is_unknown()));
        assert!(of("src").is_dir() && of("src/d0").is_dir() && of("src/sealed").is_dir());
        assert!(of("src/link").is_link() && of("src/dangling").is_link() && of("src/via").is_link());
        assert_eq!(of("src/via/f0.rs"), of("src/d0/f0.rs"));
        assert!(of("src/d0/f1.rs").executable() && !of("src/d0/f0.rs").executable());
        assert_eq!((of("src/d0/f3.rs").size, of("top.rs").size, of("src/sealed/inside.rs").size), (8, 3, 2));
        for gone in GONE {
            assert_eq!(of(gone), Stamp::GONE, "{gone}");
        }
        assert_eq!(driver.in_flight(), 0);
        drop(driver);
        unsweep(&root);
    }

    /// The allocation budget of sweeps and listings: nothing per name on a stamper; on the
    /// ring's thread, the root's name to open, the job and its result per sweep (a restart's,
    /// not the warm loop's), however many names, and for either at most one block of a
    /// stamper's channel now and then. The wake's read allocates nothing.
    #[test]
    fn a_sweep_allocates_per_sweep_not_per_name() {
        let root = dir("uring-stamp-alloc");
        let names = sweep_tree(&root, 2_000);
        let fd = open_dir(&root).unwrap();
        let mut parent = Vec::with_capacity(libc::PATH_MAX as usize);
        let mut stamps = vec![Stamp::UNKNOWN; names.len()];
        blaze_alloc_count::assert_at_most(0, "stamp_names", || {
            stamp_names(fd.as_raw_fd(), &names, 0..names.len(), &mut parent, |at, stamp| stamps[at] = stamp);
        });
        assert!(stamps.iter().all(|it| !it.is_unknown()));
        let mut driver = Uring::new(&root).unwrap();
        for round in 0..3 {
            let mut effects = Vec::with_capacity(1);
            effects.push(Effect::Stamp { files: names.clone(), tag: Tag(round) });
            // The first starts the stampers.
            match round {
                0 => driver.submit(&mut effects),
                _ => blaze_alloc_count::assert_at_most(4, "submitting a sweep", || driver.submit(&mut effects)),
            }
            let mut seen = 0;
            run_until_idle(&mut driver, &mut Vec::new(), |done, _| {
                assert!(matches!(done.done, Done::Stamped(_)), "{done:?}");
                seen += 1;
            });
            assert_eq!(seen, 1);
        }
        for round in 0..3 {
            let mut effects = Vec::with_capacity(1);
            effects.push(Effect::Entries { dir: FileName::new("src/d0").unwrap(), tag: Tag(10 + round) });
            blaze_alloc_count::assert_at_most(1, "submitting a listing", || driver.submit(&mut effects));
            let mut seen = 0;
            run_until_idle(&mut driver, &mut Vec::new(), |done, _| {
                assert!(matches!(done.done, Done::Entries(_)), "{done:?}");
                seen += 1;
            });
            assert_eq!(seen, 1);
        }
        drop(driver);
        unsweep(&root);
    }

    /// Two sweeps in flight at once, with an empty one, an append, a timer and a listing of
    /// entries among them: each ends as the reference has it. A sweep of a root not there
    /// fails, naming it.
    #[test]
    fn sweeps_in_flight_together_and_with_other_effects() {
        let root = dir("uring-stamps");
        let names = sweep_tree(&root, 6_000);
        let mut reversed = Names::default();
        for at in (0..names.len()).rev() {
            assert!(reversed.push_str(names.get(at)));
        }
        let reference = crate::fs::stamps(&root, &names).unwrap();
        let src = FileName::new("src").unwrap();
        let entries = crate::fs::entries(&root.join("src"), &src).unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let mut effects = vec![
            Effect::Stamp { files: names.clone(), tag: Tag(1) },
            Effect::Append { file: FileName::new("log").unwrap(), bytes: b"x".to_vec(), tag: Tag(2) },
            Effect::Stamp { files: Arc::new(reversed), tag: Tag(3) },
            Effect::Stamp { files: Arc::default(), tag: Tag(4) },
            Effect::Timer { after: Duration::from_millis(1), tag: Tag(5) },
            Effect::Entries { dir: src, tag: Tag(6) },
            Effect::Entries { dir: FileName::new("src/missing").unwrap(), tag: Tag(7) },
        ];
        driver.submit(&mut effects);
        assert_eq!(driver.in_flight(), 7);
        assert_eq!(driver.offload.out, 4, "two sweeps and two listings off the ring");
        let mut seen = BTreeMap::new();
        run_until_idle(&mut driver, &mut Vec::new(), |done, _| _ = seen.insert(done.tag.0, done.done));
        assert!(seen[&1] == Done::Stamped(reference.clone()), "the first sweep");
        let mut backwards = reference;
        backwards.reverse();
        assert!(seen[&3] == Done::Stamped(backwards), "the reversed sweep");
        assert_eq!(seen[&2], Done::Appended);
        assert_eq!(seen[&4], Done::Stamped(Vec::new()));
        assert_eq!(seen[&5], Done::Fired);
        assert_eq!(seen[&6], Done::Entries(entries));
        assert!(matches!(&seen[&7], Done::Failed(_)), "{:?}", seen[&7]);
        assert_eq!(driver.in_flight(), 0);
        drop(driver);

        let nowhere = dir("uring-stamp-no-root");
        let mut driver = Uring::new(&nowhere).unwrap();
        let mut effects = vec![Effect::Stamp { files: names, tag: Tag(1) }];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| seen.push(done.done));
        let [Done::Failed(why)] = &seen[..] else { panic!("{seen:?}") };
        assert!(why.0.contains(&*nowhere.to_string_lossy()), "{why}");
        unsweep(&root);
    }

    /// docs/v2/plan/design/restore.md's budget, a warm sweep of 100 000 tracked names in 50 ms,
    /// and the speed of light (docs/v2/plan/lanes/frontend/journal/linux.md): the kernel's own
    /// work for every name, measured as one thread's plain `statx` calls of them, over the
    /// cores. The sweep comes within 10% of it; the reference sweep's time is printed beside.
    #[test]
    #[ignore = "a benchmark: makes 100 000 files and times sweeps of them"]
    fn a_sweep_of_100k_names_is_within_the_restart_budget() {
        let root = dir("uring-stamp-bench");
        let names = sweep_tree(&root, 100_000);
        let mut driver = Uring::new(&root).unwrap();
        let mut best = Duration::MAX;
        for round in 0..6 {
            let began = Instant::now();
            let mut effects = vec![Effect::Stamp { files: names.clone(), tag: Tag(round) }];
            let mut stamped = 0;
            run_until_idle(&mut driver, &mut effects, |done, _| match done.done {
                Done::Stamped(stamps) => stamped = stamps.len(),
                other => panic!("{other:?}"),
            });
            assert_eq!(stamped, names.len());
            // The first warms the caches, as a daemon's earlier life has.
            if round > 0 {
                best = best.min(began.elapsed());
            }
        }
        let began = Instant::now();
        crate::fs::stamps(&root, &names).unwrap();
        let threads = began.elapsed();
        let dir = open_dir(&root).unwrap();
        let mut one = Duration::MAX;
        for _ in 0..3 {
            let began = Instant::now();
            for at in 0..names.len() {
                std::hint::black_box(statx_at(dir.as_raw_fd(), names.with_nul(at)));
            }
            one = one.min(began.elapsed());
        }
        let cores = std::thread::available_parallelism().map_or(1, |it| it.get());
        let light = one / cores as u32;
        eprintln!(
            "100 000 names: the stampers {best:?}, the speed of light {light:?} ({one:?} on one \
             core of {cores}), the reference on threads {threads:?}"
        );
        assert!(best < Duration::from_millis(50), "{best:?}");
        assert!(best.as_secs_f64() * 0.9 <= light.as_secs_f64(), "{best:?} against {light:?}");
        drop(driver);
        unsweep(&root);
    }

    /// A journal on Linux first says the changes since a saved cursor are lost (and nothing is
    /// for no cursor), then tells a file written as a live batch, before a flush asked after
    /// it; it ends at an unwatch, as a watch does.
    #[test]
    fn a_journal_tells_what_is_lost_then_what_is_written() {
        for (since, lost) in [(Cursor::Sim { journal: 1, next: 9 }, true), (Cursor::None, false)] {
            let root = dir("uring-journal");
            std::fs::create_dir_all(root.join("src")).unwrap();
            let src = FileName::new("src").unwrap();
            let mut driver = Uring::new(&root).unwrap();
            driver.submit(&mut vec![Effect::Journal { dir: src.clone(), since, tag: Tag(7) }]);
            let mut done = Vec::new();
            driver.poll(&mut done, Duration::from_secs(5));
            let lost = if lost { vec![(src.clone(), Lost::NoJournal)] } else { Vec::new() };
            match done.remove(0) {
                Completion { tag: Tag(7), done: Done::Journaled(first) } => assert_eq!(
                    *first,
                    Journaled { files: Vec::new(), lost, cursor: Cursor::None, caught_up: true }
                ),
                other => panic!("{other:?}"),
            }
            assert!(done.is_empty(), "{done:?}");
            assert_eq!(driver.in_flight(), 1);
            let written = FileName::new("src/new.rs").unwrap();
            std::fs::write(root.join(written.as_str()), b"fn f() {}").unwrap();
            driver.submit(&mut vec![Effect::Flush { watch: Tag(7), tag: Tag(8) }]);
            let (mut heard, mut flushed) = (Vec::new(), false);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !flushed && Instant::now() < deadline {
                driver.poll(&mut done, Duration::from_millis(50));
                for it in done.drain(..) {
                    match (it.tag.0, it.done) {
                        (7, Done::Journaled(batch)) => {
                            assert!(batch.caught_up && batch.lost.is_empty(), "{batch:?}");
                            heard.extend(batch.files);
                        }
                        (8, Done::Flushed) => {
                            assert!(heard.contains(&written), "flushed first; {heard:?}");
                            flushed = true;
                        }
                        other => panic!("{other:?}"),
                    }
                }
            }
            assert!(flushed, "the flush never came; heard {heard:?}");
            let mut ended = false;
            run_until_idle(&mut driver, &mut vec![Effect::Unwatch { tag: Tag(7) }], |it, _| {
                match (it.tag.0, it.done) {
                    (7, Done::Journaled(_)) => {}
                    (7, Done::Unwatched) => ended = true,
                    other => panic!("{other:?}"),
                }
            });
            assert!(ended);
            assert_eq!(driver.in_flight(), 0);
            drop(driver);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// The slab of requests answers as the map of ids it replaced does, over random starts and
    /// ends of many seeds: each `user_data` names its request and link, and one of a request
    /// that ended names nothing, even once its slot holds another; [`NOBODY`] names nothing.
    #[test]
    fn pending_agrees_with_the_map_it_replaced() {
        let made = |n: u64| Pending { tag: Tag(n), op: Op::Cancel { target: n.wrapping_mul(7) }, abandoned: false };
        let shown = |it: &Pending| match it.op {
            Op::Cancel { target } => (it.tag, target),
            _ => unreachable!("only cancels are made"),
        };
        for seed in 0..64 {
            let mut rng = crate::sim::Rng::new(seed);
            let mut ours: Slab<Pending> = Slab::new(PENDING, MemTag::Io).unwrap();
            let mut theirs: FxHashMap<u64, Pending> = FxHashMap::default();
            let (mut live, mut ended): (Vec<(u64, Id)>, Vec<Id>) = (Vec::new(), Vec::new());
            for next in 0..2_000u64 {
                if live.is_empty() || (live.len() < 256 && rng.below(3) != 0) {
                    let id = ours.insert(made(next));
                    theirs.insert(next, made(next));
                    live.push((next, id));
                } else {
                    let (key, id) = live.swap_remove(rng.below(live.len() as u64) as usize);
                    assert_eq!(ours.remove(id).as_ref().map(shown), theirs.remove(&key).as_ref().map(shown));
                    ended.push(id);
                }
                assert_eq!(ours.len(), theirs.len());
                if !live.is_empty() {
                    let (key, id) = live[rng.below(live.len() as u64) as usize];
                    let link = rng.below(4) as usize;
                    assert_eq!(of_data(data(id, link)), (id, link));
                    assert_eq!(ours.get(of_data(data(id, link)).0).map(shown), theirs.get(&key).map(shown));
                }
                if !ended.is_empty() {
                    let id = ended[rng.below(ended.len() as u64) as usize];
                    assert!(ours.get(of_data(data(id, 0)).0).is_none(), "{id:?} ended");
                }
                assert!(ours.get(of_data(NOBODY).0).is_none());
            }
            for (key, id) in &live {
                assert_eq!(ours.get(*id).map(shown), theirs.get(key).map(shown));
            }
            let reused = ended.iter().any(|old| live.iter().any(|(_, it)| it.index() == old.index()));
            assert!(reused, "seed {seed}: no slot of an ended request holds another");
        }
    }

    /// Each request in flight, by its handle.
    fn requests(driver: &Uring) -> Vec<Id> {
        driver.pending.iter().map(|(id, _)| id).collect()
    }

    /// A completion that comes after its request ended, once the request's slot holds another
    /// (a read's, a sync's, an unwatched watch's read's), is told from the other's and dropped:
    /// it completes nothing, and the other goes on to its own end.
    #[test]
    fn a_completion_of_an_ended_request_is_stale() {
        let root = dir("uring-stale");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("file"), b"bytes").unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let file = FileName::new("file").unwrap();
        driver.submit(&mut vec![Effect::Sync { file: file.clone(), tag: Tag(1) }]);
        let ids = requests(&driver);
        let [synced] = ids[..] else { panic!("{ids:?}") };
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut Vec::new(), |done, _| seen.push(done.done));
        assert_eq!(seen, [Done::Synced]);
        driver.submit(&mut vec![Effect::Read { file: file.clone(), at: 0, len: 5, tag: Tag(2) }]);
        let ids = requests(&driver);
        let [read] = ids[..] else { panic!("{ids:?}") };
        assert_eq!(read.index(), synced.index(), "the sync's slot is the read's");
        assert_ne!(read, synced);
        driver.completed(data(synced, 0), 0, 0);
        driver.completed(data(synced, 0), -libc::EIO, 0);
        assert!(driver.ready.is_empty(), "{:?}", driver.ready);
        assert!(driver.pending.contains(read));
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut Vec::new(), |done, _| seen.push(done.done));
        assert_eq!(seen, [Done::Read(b"bytes".to_vec())]);

        let src = FileName::new("src").unwrap();
        driver.submit(&mut vec![Effect::Watch { dir: src.clone(), tag: Tag(3) }]);
        let watched = driver.watches[&Tag(3)];
        let mut seen = Vec::new();
        run_until_idle(&mut driver, &mut vec![Effect::Unwatch { tag: Tag(3) }], |done, _| seen.push(done.done));
        let (last, rest) = seen.split_last().expect("unwatched");
        assert_eq!(last, &Done::Unwatched);
        assert!(rest.iter().all(|it| matches!(it, Done::Changed { .. })), "{seen:?}");
        // The two slots let go (the read's and its cancel's), both taken again.
        driver.submit(&mut vec![Effect::Watch { dir: src, tag: Tag(4) }, Effect::Sync { file, tag: Tag(5) }]);
        let reused = requests(&driver).into_iter().find(|it| it.index() == watched.index());
        let reused = reused.expect("the watch's read's slot is taken again");
        assert_ne!(reused, watched);
        driver.completed(data(watched, 0), 16, 0);
        driver.completed(data(watched, 0), -libc::ECANCELED, 0);
        assert!(driver.ready.is_empty(), "{:?}", driver.ready);
        assert!(driver.pending.contains(reused) && driver.watches.contains_key(&Tag(4)));
        let mut seen = BTreeMap::new();
        run_until_idle(&mut driver, &mut vec![Effect::Unwatch { tag: Tag(4) }], |done, _| {
            _ = seen.insert(done.tag.0, done.done)
        });
        assert_eq!((seen.get(&4), seen.get(&5)), (Some(&Done::Unwatched), Some(&Done::Synced)));
        assert_eq!(driver.in_flight(), 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Polls `driver` into `out` until `enough` says it holds what is waited for, or ten
    /// seconds pass.
    fn poll_until(driver: &mut Uring, out: &mut Vec<Completion>, mut enough: impl FnMut(&[Completion]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !enough(out) && Instant::now() < deadline {
            driver.poll(out, Duration::from_millis(50));
        }
    }

    /// The rounds before this one open what is kept and grow what is reused.
    const WARM: u64 = 3;

    /// docs/v2/plan/lanes/mem/core/linux.md's budget: a warm sync, stat and read of a file kept
    /// open allocate the read's bytes and nothing else.
    #[test]
    fn a_warm_cycle_allocates_the_read_alone() {
        let root = dir("uring-warm-file");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("cached"), b"cached bytes").unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let file = FileName::new("cached").unwrap();
        let mut out = Vec::with_capacity(16);
        for round in 0..2 * WARM {
            let mut effects = Vec::with_capacity(4);
            effects.push(Effect::Sync { file: file.clone(), tag: Tag(3 * round) });
            effects.push(Effect::Stat { file: file.clone(), tag: Tag(3 * round + 1) });
            effects.push(Effect::Read { file: file.clone(), at: 0, len: 6, tag: Tag(3 * round + 2) });
            out.clear();
            let ((), used) = blaze_alloc_count::measure(|| {
                driver.submit(&mut effects);
                poll_until(&mut driver, &mut out, |out| out.len() == 3);
            });
            out.sort_by_key(|it| it.tag.0);
            let [synced, stat, read] = &out[..] else { panic!("{out:?}") };
            assert_eq!((&synced.done, &read.done), (&Done::Synced, &Done::Read(b"cached".to_vec())));
            assert!(matches!(stat.done, Done::Stat(Some(_))), "{stat:?}");
            if round >= WARM {
                assert!(used.allocations <= 1, "round {round}: {} allocations ({} bytes), budget 1", used.allocations, used.bytes);
            }
        }
        assert_eq!(driver.in_flight(), 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A warm timer armed and fired allocates nothing.
    #[test]
    fn a_warm_timer_allocates_nothing() {
        let mut driver = Uring::new(dir("uring-warm-timer")).unwrap();
        let mut out = Vec::with_capacity(4);
        for round in 0..2 * WARM {
            let mut effects = Vec::with_capacity(1);
            effects.push(Effect::Timer { after: Duration::from_millis(1), tag: Tag(round) });
            out.clear();
            let ((), used) = blaze_alloc_count::measure(|| {
                driver.submit(&mut effects);
                poll_until(&mut driver, &mut out, |out| !out.is_empty());
            });
            assert_eq!(out.iter().map(|it| (it.tag, &it.done)).collect::<Vec<_>>(), [(Tag(round), &Done::Fired)]);
            if round >= WARM {
                assert_eq!(used.allocations, 0, "round {round}: {} bytes", used.bytes);
            }
        }
        assert_eq!(driver.in_flight(), 0);
    }

    /// A warm watch added, told of a file written, and removed allocates the payload of its
    /// [`Done::Changed`]s (each one's list and names) and nothing else: its tables, its buffer
    /// and its list of flushes are those of the watch before it.
    #[test]
    fn a_warm_watch_allocates_its_changes_alone() {
        let root = dir("uring-warm-watch");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut driver = Uring::new(&root).unwrap();
        let src = FileName::new("src").unwrap();
        let mut out = Vec::with_capacity(64);
        for round in 0..2 * WARM {
            let tag = Tag(round);
            let written = FileName::new(&format!("src/f{round}.rs")).unwrap();
            let path = root.join(written.as_str());
            let (mut watch, mut unwatch) = (Vec::with_capacity(1), Vec::with_capacity(1));
            watch.push(Effect::Watch { dir: src.clone(), tag });
            unwatch.push(Effect::Unwatch { tag });
            out.clear();
            let heard = |out: &[Completion]| {
                out.iter().any(|it| matches!(&it.done, Done::Changed { files, .. } if files.contains(&written)))
            };
            let unwatched = |out: &[Completion]| out.iter().any(|it| it.done == Done::Unwatched);
            let ((), added) = blaze_alloc_count::measure(|| driver.submit(&mut watch));
            std::fs::write(&path, b"fn f() {}").unwrap();
            let ((), told) = blaze_alloc_count::measure(|| poll_until(&mut driver, &mut out, heard));
            let ((), removed) = blaze_alloc_count::measure(|| {
                driver.submit(&mut unwatch);
                poll_until(&mut driver, &mut out, unwatched);
            });
            assert!(heard(&out) && unwatched(&out), "{out:?}");
            assert!(out.iter().all(|it| it.tag == tag), "{out:?}");
            let payload: u64 = out
                .iter()
                .map(|it| match &it.done {
                    Done::Changed { files, .. } if !files.is_empty() => 1 + files.len() as u64,
                    _ => 0,
                })
                .sum();
            let used = added.allocations + told.allocations + removed.allocations;
            if round >= WARM {
                assert!(
                    used <= payload,
                    "round {round}: {used} allocations (the add {}, its changes {}, the remove {}), \
                     the changes' payload {payload}",
                    added.allocations,
                    told.allocations,
                    removed.allocations
                );
            }
        }
        assert_eq!(driver.in_flight(), 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A warm send on a connection and a receive of it on the other side allocate the bytes
    /// each receive hands back and nothing else.
    #[test]
    fn a_warm_send_and_receive_allocate_the_bytes_received_alone() {
        let root = dir("uring-warm-conn");
        let mut ring = Uring::new(&root).unwrap();
        let at = Address::Unix(FileName::new("s.sock").unwrap());
        ring.submit(&mut vec![Effect::Listen { at: at.clone(), tag: Tag(1) }, Effect::Connect { at, tag: Tag(2) }]);
        let (mut server, mut client) = (None, None);
        let mut out = Vec::with_capacity(16);
        poll_until(&mut ring, &mut out, |out| {
            out.iter().any(|it| matches!(it.done, Done::Accepted { .. }))
                && out.iter().any(|it| matches!(it.done, Done::Connected { .. }))
        });
        for it in out.drain(..) {
            match it.done {
                Done::Accepted { conn } => server = Some(conn),
                Done::Connected { conn } => client = Some(conn),
                other => panic!("{other:?}"),
            }
        }
        let (server, client) = (server.expect("accepted"), client.expect("connected"));
        for round in 0..2 * WARM {
            let mut effects = Vec::with_capacity(8);
            effects.push(Effect::Send { conn: client, bytes: b"ping".to_vec(), tag: Tag(10 + 2 * round) });
            effects.push(Effect::Recv { conn: server, most: 64, tag: Tag(11 + 2 * round) });
            out.clear();
            let mut got = 0;
            let ((), used) = blaze_alloc_count::measure(|| {
                ring.submit(&mut effects);
                let deadline = Instant::now() + Duration::from_secs(10);
                let (mut seen, mut sent) = (0, false);
                while (got < 4 || !sent) && Instant::now() < deadline {
                    ring.poll(&mut out, Duration::from_millis(50));
                    for it in &out[seen..] {
                        match &it.done {
                            Done::Sent => sent = true,
                            Done::Read(bytes) => {
                                got += bytes.len();
                                if got < 4 {
                                    effects.push(Effect::Recv { conn: server, most: 64, tag: it.tag });
                                }
                            }
                            _ => {}
                        }
                    }
                    seen = out.len();
                    if !effects.is_empty() {
                        ring.submit(&mut effects);
                    }
                }
            });
            assert_eq!(got, 4, "{out:?}");
            let reads = out.iter().filter(|it| matches!(&it.done, Done::Read(bytes) if !bytes.is_empty())).count();
            assert!(out.iter().all(|it| matches!(it.done, Done::Sent | Done::Read(_))), "{out:?}");
            if round >= WARM {
                assert!(used.allocations <= reads as u64, "round {round}: {} allocations, {reads} receives", used.allocations);
            }
        }
        // Bits no connection was given (an even generation, as a free slot may have) name none.
        let forged = Conn(server.0 ^ 1 << 32);
        let mut seen = Vec::new();
        let mut send = vec![Effect::Send { conn: forged, bytes: b"x".to_vec(), tag: Tag(5) }];
        run_until_idle(&mut ring, &mut send, |done, _| seen.push(done.done));
        assert!(matches!(seen[..], [Done::Failed(_)]), "{seen:?}");
        let mut effects = vec![
            Effect::Hangup { conn: client, tag: Tag(3) },
            Effect::Hangup { conn: server, tag: Tag(4) },
            Effect::Unlisten { tag: Tag(1) },
        ];
        run_until_idle(&mut ring, &mut effects, |_, _| {});
        assert_eq!(ring.in_flight(), 0);
        assert!(ring.conns.is_empty());
        drop(ring);
        _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_ring_refused_by_seccomp_says_what_failed_why_and_the_fix() {
        let error = io::Error::from_raw_os_error(libc::EPERM);
        assert_eq!(
            super::setup_refused(libc::EPERM, &error, Some(0), "6.8.0"),
            "blaze runs its build on io_uring on Linux and cannot start without it: \
             io_uring_setup failed with EPERM (Operation not permitted (os error 1)). \
             Likely cause: a seccomp policy that refuses io_uring_setup, such as Docker's \
             default profile (Docker 25 and later) or a systemd unit's SystemCallFilter. \
             Fix: run under a seccomp profile that allows io_uring_setup, io_uring_enter and \
             io_uring_register (Docker: --security-opt seccomp=<that profile>)."
        );
    }

    #[test]
    fn each_refusal_names_its_errno_cause_and_fix() {
        let says = |errno: i32, disabled: Option<u8>| {
            super::setup_refused(errno, &io::Error::from_raw_os_error(errno), disabled, "5.0.0")
        };
        let cases: [(i32, Option<u8>, &str, &str, &str); 7] = [
            (libc::EPERM, Some(2), "with EPERM", "kernel.io_uring_disabled is 2", "sysctl -w kernel.io_uring_disabled=0"),
            (libc::EPERM, Some(1), "with EPERM", "kernel.io_uring_group", "sysctl -w kernel.io_uring_disabled=0"),
            (libc::EPERM, None, "with EPERM", "seccomp", "io_uring_setup, io_uring_enter and io_uring_register"),
            (libc::ENOSYS, None, "with ENOSYS", "Linux 5.0.0", "Linux 6.0 or later"),
            (libc::EINVAL, None, "with EINVAL", "IORING_SETUP_SINGLE_ISSUER", "Linux 6.0 or later"),
            (libc::ENOMEM, None, "with ENOMEM", "RLIMIT_MEMLOCK", "ulimit -l"),
            (libc::EBADF, None, "with errno 9", "not known", "kernel.io_uring_disabled is 0"),
        ];
        for (errno, disabled, failed, cause, fix) in cases {
            let text = says(errno, disabled);
            let (head, rest) = text.split_once(" Likely cause: ").expect("a cause");
            let (because, how) = rest.split_once(" Fix: ").expect("a fix");
            assert!(head.contains(failed), "{text}");
            assert!(because.contains(cause), "{text}");
            assert!(how.contains(fix), "{text}");
        }
    }
}
