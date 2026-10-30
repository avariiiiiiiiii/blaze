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
    cell::RefCell,
    collections::VecDeque,
    ffi::CString,
    io,
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        unix::ffi::OsStrExt as _,
    },
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU16, AtomicU32, Ordering, fence},
        mpsc,
    },
    time::{Duration, Instant},
};

use rustc_hash::FxHashMap;

use crate::{
    Command, Completion, Confine, Conn, Done, Driver, Effect, FileName, IoError, Tag,
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

/// Requests the ring holds at once, enough to keep an NVMe queue full; more wait in the
/// driver until there is room.
const ENTRIES: u32 = 4096;
/// Completions it holds unreaped: a multishot request may post many for one request.
const COMPLETIONS: u32 = 4 * ENTRIES;

/// What one read of a spawned process's pipe takes at most.
const PIPE: usize = 64 << 10;

/// The `user_data` of a request whose completion is nobody's.
const NOBODY: u64 = u64::MAX;

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
            format!("io_uring {what}: {error}: Linux {since} or later has it, and this is Linux {}", crate::cgroup::kernel()),
        ),
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
            return Err(io::Error::other(format!("RLIMIT_NOFILE is {}: no table of files fits", limit.rlim_cur)));
        }
        let fd = ring.as_raw_fd();
        let sparse = RsrcRegister { nr: size, flags: RSRC_REGISTER_SPARSE, resv2: 0, data: 0, tags: 0 };
        let nr = size_of::<RsrcRegister>() as libc::c_uint;
        register(fd, REGISTER_FILES2, &raw const sparse, nr, "a sparse table of registered files", "5.19")?;
        let ours = size / 2;
        let range = FileIndexRange { off: ours, len: size - ours, resv: 0 };
        register(fd, REGISTER_FILE_ALLOC_RANGE, &raw const range, 0, "a range of the file table for direct accepts", "6.0")?;
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
    fn install(self: &Rc<Table>, fd: OwnedFd) -> io::Result<Rc<Fixed>> {
        let slot = self.free.borrow_mut().pop();
        if let Some(slot) = slot
            && let Err(error) = self.set(slot, fd.as_raw_fd())
        {
            self.free.borrow_mut().push(slot);
            return Err(error);
        }
        Ok(Rc::new(Fixed { fd: Some(fd), slot, table: self.clone() }))
    }

    /// The connection an accept put in slot `slot` of the kernel's half.
    fn accepted(self: &Rc<Table>, slot: u32) -> Rc<Fixed> {
        Rc::new(Fixed { fd: None, slot: Some(slot), table: self.clone() })
    }
}

/// A file the ring's requests name: its slot of the [`Table`] where it has one, else its
/// descriptor. Shared by each request in flight on it, so its slot is emptied, and may be
/// reused, only once no request can name it any more.
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
            .collect();
        let buffers = RsrcRegister { nr: DIRECT_BUFFERS.into(), flags: 0, resv2: 0, data: iovecs.as_ptr() as u64, tags: 0 };
        let nr = size_of::<RsrcRegister>() as libc::c_uint;
        register(ring, REGISTER_BUFFERS2, &raw const buffers, nr, "registered buffers", "5.13").map_err(|error| {
            match error.raw_os_error() {
                Some(libc::ENOMEM) => io::Error::new(
                    error.kind(),
                    format!(
                        "{error}: pinning {} MiB of buffers needs RLIMIT_MEMLOCK of that much (ulimit -l) or CAP_IPC_LOCK",
                        DIRECT_BUFFERS as usize * DIRECT_PIECE >> 20
                    ),
                ),
                _ => error,
            }
        })?;
        Ok(Registered { map, free: (0..DIRECT_BUFFERS).rev().collect() })
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
            if error.raw_os_error() == Some(libc::EINVAL) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "io_uring_setup with IORING_SETUP_SQPOLL and IORING_SETUP_SINGLE_ISSUER: \
                         {error}: Linux 6.0 or later has them, and this is Linux {}",
                        crate::cgroup::kernel()
                    ),
                ));
            }
            return Err(error);
        }
        // SAFETY: the call returned a file descriptor of its own.
        let fd = Rc::new(unsafe { OwnedFd::from_raw_fd(fd as RawFd) });
        let wanted = [
            (FEAT_SINGLE_MMAP, "IORING_FEAT_SINGLE_MMAP", "5.4"),
            (FEAT_NODROP, "IORING_FEAT_NODROP", "5.5"),
            (FEAT_SQPOLL_NONFIXED, "IORING_FEAT_SQPOLL_NONFIXED", "5.11"),
            (FEAT_EXT_ARG, "IORING_FEAT_EXT_ARG", "5.11"),
        ];
        if let Some((_, name, since)) = wanted.iter().find(|(bit, ..)| params.features & bit == 0) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
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
            _maps: vec![rings, sqes],
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

/// A request in flight. Every buffer or address the kernel is given is on the heap and stays
/// where it is until the request's completion is reaped: moving an `Op` moves none of them. A
/// file it names is an `Rc<Fixed>` of its own, so neither the file's slot nor its descriptor is
/// let go while the kernel may still be given the request.
enum Op {
    /// Into `bytes` from `at`, `got` of `bytes.len()` read so far.
    Read {
        file: Rc<Fixed>,
        at: u64,
        bytes: Vec<u8>,
        got: usize,
    },
    /// `bytes` at the end of the file, `put` of them written so far.
    Append {
        file: Rc<Fixed>,
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
        file: Rc<Fixed>,
    },
    /// A whole file as one chain of linked requests: written to `temporary`, synced, renamed
    /// over the path (`names` are the two), and its directory `dir` synced. A link that fails
    /// cancels those after it; a short write asks the chain again from where it stopped.
    Write {
        fd: OwnedFd,
        dir: OwnedFd,
        bytes: Vec<u8>,
        put: usize,
        temporary: PathBuf,
        names: Box<[CString; 2]>,
        file: FileName,
        chain: Chain,
    },
    /// `names[0]` renamed over `names[1]`, then each of `dirs` synced, linked.
    Rename {
        names: Box<[CString; 2]>,
        dirs: Vec<OwnedFd>,
        chain: Chain,
    },
    /// A read of a watch's events, asked again after each until the watch is stopped.
    Watch {
        tree: crate::watch::Tree,
        buffer: Vec<u8>,
        stopping: bool,
    },
    /// Stops the request whose `user_data` is `target`; its own completion is nobody's.
    Cancel {
        target: u64,
    },
    /// The exit of the process of the spawn of its tag: a poll of the pidfd its [`Spawning`]
    /// keeps open for as long as any request of it is in flight.
    Exit {
        pidfd: RawFd,
    },
    /// One of a spawn's output pipes, read until it ends.
    Output {
        fd: OwnedFd,
        buffer: Vec<u8>,
        stderr: bool,
    },
    /// A spawn's input, `put` of it written; its pipe is closed when this ends.
    Input {
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
    /// The kernel reads the address from `at` until it completes.
    Connect {
        socket: Option<Rc<Fixed>>,
        at: Box<crate::net::SockAddr>,
    },
    /// The one receive of `conn`, multishot: each completion's bytes are in a buffer of the
    /// [`Provided`] ring, copied into the connection's inbox and the buffer given back.
    Recv {
        conn: Conn,
        socket: Rc<Fixed>,
    },
    /// A send on `conn`, `put` of `bytes` sent so far.
    Send {
        conn: Conn,
        socket: Rc<Fixed>,
        bytes: Vec<u8>,
        put: usize,
    },
    /// `conn` shut both ways, for a hangup.
    Shutdown {
        conn: Conn,
        socket: Rc<Fixed>,
    },
    /// A read of the eventfd that threads doing work off the ring bump once they have sent
    /// its completion; in flight while any such work is.
    Wake {
        fd: RawFd,
        count: Box<[u8; 8]>,
    },
}

impl Op {
    /// The completions the kernel still owes it: one per link of a chain not yet in.
    fn owed(&self) -> usize {
        match self {
            Op::Write { chain, .. } => chain.owed(WRITE_LINKS),
            Op::Rename { chain, dirs, .. } => chain.owed(1 + dirs.len()),
            _ => 1,
        }
    }
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

/// The `user_data` of link `link` of the request `id`; a request of no chain is its link 0.
fn data(id: u64, link: usize) -> u64 {
    id << 2 | link as u64
}

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
    reading: Option<u64>,
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
        let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
        Ok(Offload { sender, receiver, wake, out: 0, reading: None })
    }
}

struct Pending {
    tag: Tag,
    op: Op,
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
        let program = finish.command.program.display().to_string();
        // Reaps it, and removes what was made for it even where it failed whatever it says.
        let exited = finish.exited(pid, stdout, stderr);
        drop(pidfd);
        match (exited, failed) {
            (_, Some(why)) => Done::Failed(IoError(format!("{program}: {why}"))),
            (Ok(exit), None) => Done::Exited(Box::new(exit)),
            (Err(error), None) => Done::Failed(IoError(error.to_string())),
        }
    }
}

/// A connection a listen accepted or a connect made.
struct Connection {
    socket: Rc<Fixed>,
    /// Requests in flight on it (its receive, sends, its shutdown): it is let go once none is
    /// and it is hung up.
    users: usize,
    hung: bool,
    /// What its receive took and no `Recv` has yet, in order.
    inbox: VecDeque<u8>,
    /// The `Recv`s waiting for bytes, in the order they were submitted, with their most.
    recvs: VecDeque<(Tag, usize)>,
    /// Its multishot receive, while the kernel holds one.
    receiving: Option<u64>,
    /// Its receive is being cancelled: the inbox holds [`INBOX`] bytes no `Recv` took.
    pausing: bool,
    /// How its receiving ended: the peer hung up (or reset it), or why it failed.
    ended: Option<Result<(), String>>,
}

pub struct Uring {
    start: Instant,
    root: PathBuf,
    ring: Ring,
    table: Rc<Table>,
    registered: Registered,
    /// Not freed while the kernel may still write into it (see `Drop`).
    provided: std::mem::ManuallyDrop<Provided>,
    /// Each file opened, for reading and appending.
    files: FxHashMap<FileName, Rc<Fixed>>,
    /// By the id in the `user_data` the ring hands back, which is never used for another
    /// request.
    pending: FxHashMap<u64, Pending>,
    next: u64,
    /// Requests for which the ring had no room.
    waiting: VecDeque<u64>,
    /// Writes bypassing the page cache waiting for a registered buffer.
    for_buffers: VecDeque<u64>,
    /// Devices whose file system refused `O_DIRECT`: their writes go through the page cache.
    refused: rustc_hash::FxHashSet<u64>,
    /// Writes that bypassed the page cache, ended.
    direct_writes: u64,
    /// Completions to hand out at the next poll, in order.
    ready: Vec<Completion>,
    /// The completions reaped from the ring, kept for the next reap's.
    posted: Vec<(u64, i32, u32)>,
    /// The request reading each watch's events, by the watch's tag.
    watches: FxHashMap<Tag, u64>,
    /// The request accepting each listen's connections, by the listen's tag.
    listens: FxHashMap<Tag, u64>,
    spawns: FxHashMap<Tag, Spawning>,
    conns: FxHashMap<Conn, Connection>,
    next_conn: u64,
    /// By ticks since `start`; a wait of the ring ends at the next one due.
    timers: Wheel<Tag>,
    confine: Option<Arc<dyn Confine>>,
    /// Requests the kernel may still hold after the ring failed: kept, never freed, so it
    /// writes into nothing else.
    abandoned: Vec<Pending>,
    offload: Offload,
    in_flight: usize,
}

impl Uring {
    /// A driver whose files are under `root`, or why this kernel has no ring for it.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Uring> {
        let ring = Ring::new(ENTRIES, COMPLETIONS)?;
        let table = Table::new(&ring.fd)?;
        let registered = Registered::new(ring.fd.as_raw_fd())?;
        let provided = std::mem::ManuallyDrop::new(Provided::new(ring.fd.as_raw_fd())?);
        Ok(Uring {
            start: Instant::now(),
            root: root.into(),
            ring,
            table,
            registered,
            provided,
            files: FxHashMap::default(),
            pending: FxHashMap::default(),
            next: 0,
            waiting: VecDeque::new(),
            for_buffers: VecDeque::new(),
            refused: rustc_hash::FxHashSet::default(),
            direct_writes: 0,
            ready: Vec::new(),
            posted: Vec::new(),
            watches: FxHashMap::default(),
            listens: FxHashMap::default(),
            spawns: FxHashMap::default(),
            conns: FxHashMap::default(),
            next_conn: 0,
            timers: Wheel::new(),
            confine: None,
            abandoned: Vec::new(),
            offload: Offload::new()?,
            in_flight: 0,
        })
    }

    /// The driver, making the sandbox of each spawn whose [`crate::Sandbox`] is on with
    /// `confine`.
    pub fn confined(mut self, confine: Arc<dyn Confine>) -> Uring {
        self.confine = Some(confine);
        self
    }

    /// Where the listen `listen` listens, while it does: the port a TCP listen of port 0 was
    /// given. `None` for a Unix socket's.
    pub fn local_addr(&self, listen: Tag) -> Option<std::net::SocketAddr> {
        let id = self.listens.get(&listen)?;
        match &self.pending.get(id)?.op {
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
        self.files.remove(name);
    }

    /// The file, opened once and put in the driver's half of the table; `make` makes it if
    /// missing, with its directories. Where that half is full, the files kept are let go
    /// (each request in flight keeps its own until it ends) to make room.
    fn file(&mut self, name: &FileName, make: bool) -> io::Result<Rc<Fixed>> {
        if let Some(file) = self.files.get(name) {
            return Ok(file.clone());
        }
        let path = self.root.join(name.as_str());
        if make && let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let fd = open(&path, make)?;
        if self.table.free.borrow().is_empty() {
            self.files.clear();
        }
        let file = self.table.install(fd)?;
        self.files.insert(name.clone(), file.clone());
        Ok(file)
    }

    fn start(&mut self, tag: Tag, op: Op) -> u64 {
        let id = self.next;
        self.next += 1;
        self.pending.insert(id, Pending { tag, op });
        self.queue(id);
        id
    }

    /// Writes the request `id` as it now stands to the ring, or keeps it for when there is
    /// room (or, bypassing the page cache, for when a registered buffer is free).
    fn queue(&mut self, id: u64) {
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
    fn buffered(&mut self, id: u64) -> bool {
        let Some(Pending { op: Op::WriteAt { direct: true, buffer, .. }, .. }) =
            self.pending.get_mut(&id)
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
            if self.pending.contains_key(&id) {
                self.queue(id);
                return;
            }
        }
    }

    /// The requests the request `id` is, as it now stands, into `sqes`: one, or each link of
    /// a chain. How many.
    fn sqes(&self, id: u64, sqes: &mut [Sqe; WRITE_LINKS]) -> usize {
        let pending = &self.pending[&id];
        let user_data = data(id, 0);
        // SAFETY of the kernel's reads and writes: every buffer and address below stays where
        // it is until the request's completion (see `Op`).
        sqes[0] = match &pending.op {
            Op::Read { file, at, bytes, got } => {
                let (fd, flags) = file.target();
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
                let (fd, flags) = file.target();
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
                let (fd, flags) = file.target();
                Sqe { opcode: OP_FSYNC, flags, fd, user_data, ..Sqe::default() }
            }
            Op::Write { fd, dir, bytes, put, names, .. } => {
                let fd = fd.as_raw_fd();
                sqes[1] =
                    Sqe { opcode: OP_FSYNC, flags: SQE_IO_LINK, fd, user_data: data(id, 1), ..Sqe::default() };
                sqes[2] = rename(names, SQE_IO_LINK, data(id, 2));
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
            Op::Rename { names, dirs, .. } => {
                for (n, dir) in dirs.iter().enumerate() {
                    let flags = if n + 1 < dirs.len() { SQE_IO_LINK } else { 0 };
                    sqes[1 + n] = Sqe {
                        opcode: OP_FSYNC,
                        flags,
                        fd: dir.as_raw_fd(),
                        user_data: data(id, 1 + n),
                        ..Sqe::default()
                    };
                }
                sqes[0] = rename(names, if dirs.is_empty() { 0 } else { SQE_IO_LINK }, user_data);
                return 1 + dirs.len();
            }
            Op::Watch { tree, buffer, .. } => Sqe {
                opcode: OP_READ,
                fd: tree.raw(),
                off: u64::MAX,
                addr: buffer.as_ptr() as u64,
                len: buffer.len() as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Cancel { target } => Sqe {
                opcode: OP_ASYNC_CANCEL,
                fd: -1,
                addr: *target,
                user_data,
                ..Sqe::default()
            },
            Op::Exit { pidfd } => Sqe {
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
            Op::Input { fd, bytes, put } => Sqe {
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
            Op::Recv { socket, .. } => {
                let (fd, flags) = socket.target();
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
            Op::Send { socket, bytes, put, .. } => {
                let (fd, flags) = socket.target();
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
            Op::Shutdown { socket, .. } => {
                let (fd, flags) = socket.target();
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
                addr: count.as_ptr() as u64,
                len: count.len() as u32,
                user_data,
                ..Sqe::default()
            },
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
        let root = self.root.clone();
        let sender = self.offload.sender.clone();
        let wake = Arc::clone(&self.offload.wake);
        std::thread::Builder::new().name("blaze-uring-off".into()).spawn(move || {
            let done = crate::threads::run(&root, effect)
                .unwrap_or_else(|error| Done::Failed(IoError(error.to_string())));
            // A driver dropped meanwhile takes nothing.
            if sender.send(Completion { tag, done }).is_ok() {
                let one = 1u64.to_ne_bytes();
                // SAFETY: eight bytes, which an eventfd write takes; the descriptor is kept
                // open by this thread's `Arc`.
                unsafe { libc::write(wake.as_raw_fd(), one.as_ptr().cast(), one.len()) };
            }
        })?;
        self.offload.out += 1;
        if self.offload.reading.is_none() {
            let fd = self.offload.wake.as_raw_fd();
            let id = self.start(tag, Op::Wake { fd, count: Box::new([0; 8]) });
            self.offload.reading = Some(id);
        }
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
            if !self.pending.contains_key(&id) {
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
    /// still hold of them is kept in `abandoned`, never freed.
    fn fail_unsubmitted(&mut self, error: io::Error) {
        let why = format!("the ring failed: {error}");
        let failed = |tag| Completion { tag, done: Done::Failed(IoError(why.clone())) };
        let pending: Vec<Pending> = self.pending.drain().map(|(_, it)| it).collect();
        for pending in pending {
            match pending.op {
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
            self.abandoned.push(pending);
        }
        // Timers need no ring, but end with it as everything in flight does.
        self.timers.drain(|_, tag| self.ready.push(failed(tag)));
        let mut spawns: Vec<(Tag, Spawning)> = self.spawns.drain().collect();
        spawns.sort_unstable_by_key(|it| it.0);
        for (tag, mut spawn) in spawns {
            if !spawn.exited {
                _ = kill(spawn.pidfd.as_raw_fd());
            }
            spawn.failed = Some(why.clone());
            self.ready.push(Completion { tag, done: spawn.end() });
        }
        let mut conns: Vec<Conn> = self.conns.keys().copied().collect();
        conns.sort_unstable();
        for conn in conns {
            let conn = self.conns.get_mut(&conn).expect("listed");
            for (tag, _) in conn.recvs.drain(..) {
                self.ready.push(failed(tag));
            }
            conn.users = 0;
            conn.receiving = None;
        }
        self.conns.retain(|_, conn| !conn.hung);
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
    fn completed(&mut self, user_data: u64, res: i32, flags: u32) {
        let (id, link) = (user_data >> 2, (user_data & 3) as usize);
        let Some(mut pending) = self.pending.remove(&id) else { return };
        let tag = pending.tag;
        let more = flags & CQE_F_MORE != 0;
        match self.step(tag, &mut pending.op, link, res, flags) {
            Step::Again => {
                self.pending.insert(id, pending);
                if !more {
                    self.queue(id);
                }
            }
            Step::Goes(done) => {
                self.ready.push(Completion { tag, done });
                self.pending.insert(id, pending);
                if !more {
                    self.queue(id);
                }
            }
            Step::Held => {
                self.pending.insert(id, pending);
            }
            Step::Ends(done) => {
                if let Some(done) = done {
                    self.ready.push(Completion { tag, done });
                }
                match pending.op {
                    Op::Exit { .. } | Op::Output { .. } | Op::Input { .. } => {
                        // Closes the pipe before the process is reaped.
                        drop(pending);
                        self.part_ended(tag);
                    }
                    Op::Recv { conn, .. } => {
                        drop(pending);
                        self.release(conn);
                        self.deliver(conn);
                    }
                    Op::Send { conn, .. } | Op::Shutdown { conn, .. } => {
                        drop(pending);
                        self.release(conn);
                    }
                    Op::WriteAt { direct, buffer, .. } => {
                        self.direct_writes += u64::from(direct);
                        if let Some(buffer) = buffer {
                            self.unbuffer(buffer);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// What the result `res` of link `link` of a request of the effect `tag` means for it.
    fn step(&mut self, tag: Tag, op: &mut Op, link: usize, res: i32, flags: u32) -> Step {
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
            Op::Write { bytes, put, temporary, file, chain, .. } => {
                let Some([wrote, synced, renamed, dir]) = chain.take(link, res, WRITE_LINKS) else {
                    return Step::Held;
                };
                // A short write breaks the chain; the rest is asked again, with the links
                // after it.
                if wrote > 0 && *put + (wrote as usize) < bytes.len() {
                    *put += wrote as usize;
                    return Step::Again;
                }
                let file = file.clone();
                self.forget(&file);
                let wrote = if wrote == 0 && *put < bytes.len() { -libc::EIO } else { wrote };
                match [wrote, synced, renamed].into_iter().find(|it| *it < 0) {
                    // The temporary was never renamed: the old file is as it was.
                    Some(res) => {
                        _ = std::fs::remove_file(&*temporary);
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
            Op::Watch { stopping, .. } if res == -libc::ECANCELED || (res < 0 && *stopping) => {
                self.watches.remove(&tag);
                Step::Ends(Some(Done::Unwatched))
            }
            Op::Watch { tree, buffer, stopping } if res > 0 => {
                let done = tree.take(&buffer[..res as usize]);
                if !*stopping {
                    return Step::Goes(done);
                }
                // The read was done before the cancel reached it: this change, then the end.
                self.watches.remove(&tag);
                self.ready.push(Completion { tag, done });
                Step::Ends(Some(Done::Unwatched))
            }
            Op::Watch { .. } => {
                self.watches.remove(&tag);
                Step::Ends(failed(if res < 0 { res } else { -libc::EIO }))
            }
            Op::Exit { .. } => {
                if res < 0 {
                    self.spoil(tag, format!("waiting for its exit: {}", error(res)));
                } else if let Some(spawn) = self.spawns.get_mut(&tag) {
                    spawn.exited = true;
                }
                Step::Ends(None)
            }
            Op::Output { buffer, stderr, .. } => {
                if res > 0 {
                    if let Some(spawn) = self.spawns.get_mut(&tag) {
                        let to = if *stderr { &mut spawn.stderr } else { &mut spawn.stdout };
                        to.extend_from_slice(&buffer[..res as usize]);
                    }
                    return Step::Again;
                }
                if res < 0 {
                    let pipe = if *stderr { "stderr" } else { "stdout" };
                    self.spoil(tag, format!("reading its {pipe}: {}", error(res)));
                }
                Step::Ends(None)
            }
            Op::Input { bytes, put, .. } => {
                // It ended, or closed its input, before reading all of it.
                if res == -libc::EPIPE {
                    return Step::Ends(None);
                }
                if res < 0 || (res == 0 && *put < bytes.len()) {
                    let why = if res < 0 { error(res) } else { error(-libc::EIO) };
                    self.spoil(tag, format!("writing its input: {why}"));
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
            Op::Recv { conn, .. } => {
                let conn = *conn;
                if flags & CQE_F_BUFFER != 0 {
                    let bid = (flags >> CQE_BUFFER_SHIFT) as u16;
                    if res > 0
                        && let Some(connection) = self.conns.get_mut(&conn)
                        && !connection.hung
                    {
                        connection.inbox.extend(self.provided.bytes(bid, res as usize));
                    }
                    self.provided.give(bid);
                }
                let Some(connection) = self.conns.get_mut(&conn) else {
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
    fn stop(&mut self, id: u64) {
        let Some(pending) = self.pending.get_mut(&id) else { return };
        let (Op::Watch { stopping, .. } | Op::Accept { stopping, .. }) = &mut pending.op else {
            unreachable!("a watch's or a listen's request")
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
        let pending = self.pending.remove(&id).expect("in flight");
        let done = match pending.op {
            Op::Watch { .. } => {
                self.watches.remove(&tag);
                Done::Unwatched
            }
            _ => {
                self.listens.remove(&tag);
                Done::Unlistened
            }
        };
        self.ready.push(Completion { tag, done });
    }

    /// An effect that fails as it is submitted, with no request of its own to end.
    fn refuse(&mut self, tag: Tag, why: String) {
        self.in_flight += 1;
        self.ready.push(Completion { tag, done: Done::Failed(IoError(why)) });
    }

    fn spawn(&mut self, tag: Tag, command: Box<Command>) -> io::Result<()> {
        let crate::process::Started { pid, pidfd, stdin, stdout, stderr, finish } =
            crate::process::start(&self.root, command, self.confine.as_ref())?;
        let input = finish.command.stdin.clone();
        let exit = pidfd.as_raw_fd();
        let parts = 3 + usize::from(stdin.is_some());
        self.spawns.insert(tag, Spawning {
            pid,
            pidfd,
            finish,
            stdout: Vec::new(),
            stderr: Vec::new(),
            parts,
            exited: false,
            failed: None,
        });
        self.start(tag, Op::Exit { pidfd: exit });
        self.start(tag, Op::Output { fd: stdout, buffer: vec![0; PIPE], stderr: false });
        self.start(tag, Op::Output { fd: stderr, buffer: vec![0; PIPE], stderr: true });
        if let Some(fd) = stdin {
            self.start(tag, Op::Input { fd, bytes: input, put: 0 });
        }
        Ok(())
    }

    /// A request of the spawn `tag` ended; once all have, it completes.
    fn part_ended(&mut self, tag: Tag) {
        let Some(spawn) = self.spawns.get_mut(&tag) else { return };
        spawn.parts -= 1;
        if spawn.parts == 0 {
            let spawn = self.spawns.remove(&tag).expect("in flight");
            self.ready.push(Completion { tag, done: spawn.end() });
        }
    }

    /// The spawn `tag` completes failed, saying `why`; its process is killed so that it ends.
    fn spoil(&mut self, tag: Tag, why: String) {
        let Some(spawn) = self.spawns.get_mut(&tag) else { return };
        spawn.failed.get_or_insert(why);
        if !spawn.exited {
            _ = kill(spawn.pidfd.as_raw_fd());
        }
    }

    fn kill(&mut self, tag: Tag) {
        let Some(spawn) = self.spawns.get(&tag) else { return };
        if spawn.exited {
            return;
        }
        if let Err(error) = kill(spawn.pidfd.as_raw_fd()) {
            self.spoil(tag, format!("killing it: {error}"));
        }
    }

    fn add(&mut self, socket: Rc<Fixed>) -> Conn {
        let conn = Conn(self.next_conn);
        self.next_conn += 1;
        self.conns.insert(conn, Connection {
            socket,
            users: 0,
            hung: false,
            inbox: VecDeque::new(),
            recvs: VecDeque::new(),
            receiving: None,
            pausing: false,
            ended: None,
        });
        conn
    }

    /// The socket of `conn`, kept until [`Uring::release`] says the request using it ended.
    fn use_conn(&mut self, conn: Conn) -> io::Result<Rc<Fixed>> {
        let connection = self.conns.get_mut(&conn).filter(|it| !it.hung).ok_or_else(|| gone(conn))?;
        connection.users += 1;
        Ok(connection.socket.clone())
    }

    fn release(&mut self, conn: Conn) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        connection.users -= 1;
        if connection.hung && connection.users == 0 {
            self.conns.remove(&conn);
        }
    }

    /// A receive of at most `most` bytes on `conn`: from what its receive took, in order,
    /// once there is any.
    fn recv(&mut self, conn: Conn, tag: Tag, most: usize) -> io::Result<()> {
        let connection = self.conns.get_mut(&conn).filter(|it| !it.hung).ok_or_else(|| gone(conn))?;
        connection.recvs.push_back((tag, most));
        self.deliver(conn);
        Ok(())
    }

    /// Hands `conn`'s waiting `Recv`s what its inbox holds, or its end, and arms its receive
    /// where one waits for bytes not yet taken.
    fn deliver(&mut self, conn: Conn) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        while let Some(&(tag, most)) = connection.recvs.front() {
            let done = match &connection.ended {
                _ if most == 0 => Done::Read(Vec::new()),
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
                Some(Ok(())) => Done::Read(Vec::new()),
                Some(Err(why)) => Done::Failed(IoError(why.clone())),
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
        let socket = connection.socket.clone();
        let id = self.start(NOBODYS, Op::Recv { conn, socket });
        if let Some(connection) = self.conns.get_mut(&conn) {
            connection.receiving = Some(id);
        }
    }

    /// Shuts `conn` both ways through the ring, its waiting receives answered empty, and lets
    /// it go once nothing in flight uses it.
    fn hang_up(&mut self, conn: Conn, tag: Tag) -> io::Result<()> {
        let connection = self.conns.get_mut(&conn).filter(|it| !it.hung).ok_or_else(|| gone(conn))?;
        connection.hung = true;
        connection.inbox = VecDeque::new();
        connection.users += 1;
        let socket = connection.socket.clone();
        let receiving = connection.receiving;
        for (tag, _) in std::mem::take(&mut connection.recvs) {
            self.ready.push(Completion { tag, done: Done::Read(Vec::new()) });
        }
        if let Some(id) = receiving {
            self.start(NOBODYS, Op::Cancel { target: data(id, 0) });
        }
        self.start(tag, Op::Shutdown { conn, socket });
        Ok(())
    }

    /// Writes that bypassed the page cache (`O_DIRECT`) and ended, since the driver was made.
    pub fn direct_writes(&self) -> u64 {
        self.direct_writes
    }
}

/// The rename of `names[0]` over `names[1]`, both relative to the working directory.
fn rename(names: &[CString; 2], flags: u8, user_data: u64) -> Sqe {
    Sqe {
        opcode: OP_RENAMEAT,
        flags,
        fd: libc::AT_FDCWD,
        addr: names[0].as_ptr() as u64,
        // The new name's directory, and the new name.
        len: libc::AT_FDCWD as u32,
        off: names[1].as_ptr() as u64,
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
        for spawn in self.spawns.values() {
            if !spawn.exited {
                _ = kill(spawn.pidfd.as_raw_fd());
            }
        }
        // Never handed to the kernel.
        for id in std::mem::take(&mut self.waiting).into_iter().chain(std::mem::take(&mut self.for_buffers)) {
            self.pending.remove(&id);
        }
        let mut owed: FxHashMap<u64, usize> =
            self.pending.iter().map(|(&id, it)| (id, it.op.owed())).collect();
        let deadline = Instant::now() + LETTING_GO;
        let mut posted = Vec::new();
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
                let id = user_data >> 2;
                match self.pending.get(&id).map(|it| &it.op) {
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
                        self.pending.remove(&id);
                    }
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let lent = self.pending.values().any(|it| !matches!(it.op, Op::Cancel { .. }));
            if !lent || left.is_zero() {
                break;
            }
            alive = self.ring.enter(Some(left)).is_ok();
            self.ring.reap(|user_data, res, flags| posted.push((user_data, res, flags)));
        }
        for (_, spawn) in std::mem::take(&mut self.spawns) {
            _ = spawn.end();
        }
        let lent = self.pending.values().any(|it| !matches!(it.op, Op::Cancel { .. }))
            || !self.abandoned.is_empty();
        if !lent {
            // SAFETY: dropped once, here; no request the kernel holds writes into it.
            unsafe { std::mem::ManuallyDrop::drop(&mut self.provided) };
        }
        std::mem::forget(std::mem::take(&mut self.pending));
        std::mem::forget(std::mem::take(&mut self.abandoned));
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
        format!("{conn:?}: no such connection (never made, or hung up)"),
    )
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))
}

fn open(path: &Path, make: bool) -> io::Result<OwnedFd> {
    let path = c_path(path)?;
    let flags =
        libc::O_RDWR | libc::O_APPEND | libc::O_CLOEXEC | if make { libc::O_CREAT } else { 0 };
    // SAFETY: a NUL-terminated path; the descriptor returned is this driver's.
    let fd = unsafe { libc::open(path.as_ptr(), flags, 0o644 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A file made for writing, that was not there.
fn create(path: &Path) -> io::Result<OwnedFd> {
    open_to_write(path, libc::O_CREAT | libc::O_EXCL)
}

/// A file opened for writing where it is told to; `flags` added to the opening's.
fn open_to_write(path: &Path, flags: libc::c_int) -> io::Result<OwnedFd> {
    let path = c_path(path)?;
    let flags = libc::O_WRONLY | libc::O_CLOEXEC | flags;
    // SAFETY: a NUL-terminated path; the descriptor returned is this driver's.
    let fd = unsafe { libc::open(path.as_ptr(), flags, 0o644 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The directory `path` is in, opened to be synced.
fn open_dir_of(path: &Path) -> io::Result<OwnedFd> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let dir = c_path(dir)?;
    // SAFETY: a NUL-terminated path; the descriptor returned is this driver's.
    let fd = unsafe { libc::open(dir.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
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
fn connect_socket(table: &Rc<Table>, domain: libc::c_int) -> io::Result<Rc<Fixed>> {
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
                        None => self.refuse(tag, format!("nothing is watched as {tag:?}")),
                    }
                    continue;
                }
                Effect::Unlisten { .. } => {
                    match self.listens.get(&tag).copied() {
                        Some(id) => self.stop(id),
                        None => self.refuse(tag, format!("nothing is listened on as {tag:?}")),
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
            let root = self.root.clone();
            let at = |file: &FileName| root.join(file.as_str());
            let started = match effect {
                Effect::Write { file, bytes, .. } => {
                    let path = at(&file);
                    let temporary = crate::fs::temporary_next_to(&path);
                    crate::fs::make_parent(&path)
                        .and_then(|()| Ok((c_path(&temporary)?, c_path(&path)?)))
                        .and_then(|names| Ok((names, open_dir_of(&path)?)))
                        .and_then(|(names, dir)| Ok((names, dir, create(&temporary)?)))
                        .map(|((from, to), dir, fd)| {
                            self.start(tag, Op::Write {
                                fd,
                                dir,
                                bytes,
                                put: 0,
                                temporary,
                                names: Box::new([from, to]),
                                file,
                                chain: Chain::default(),
                            });
                            None
                        })
                }
                Effect::Clone { from, to, .. } => {
                    self.forget(&to);
                    crate::fs::clone(&at(&from), &at(&to)).map(|()| Some(Done::Cloned))
                }
                Effect::Link { from, to, .. } => {
                    self.forget(&to);
                    crate::fs::link(&at(&from), &at(&to)).map(|()| Some(Done::Linked))
                }
                Effect::Stat { file, .. } => crate::fs::stat(&at(&file)).map(|it| Some(Done::Stat(it))),
                list @ Effect::List { .. } => self.off_ring(list).map(|()| None),
                Effect::WriteAt { file, at: offset, bytes, .. } => {
                    open_to_write(&at(&file), 0).and_then(|fd| match bytes.is_empty() {
                        true => Ok(Some(Done::Written)),
                        false => {
                            let direct = self.direct(fd.as_raw_fd(), offset, bytes.len())?;
                            let op = Op::WriteAt { fd, at: offset, bytes, put: 0, direct, buffer: None };
                            self.start(tag, op);
                            Ok(None)
                        }
                    })
                }
                Effect::Rename { from, to, .. } => {
                    self.forget(&from);
                    self.forget(&to);
                    let (from, to) = (at(&from), at(&to));
                    crate::fs::make_parent(&to).and_then(|()| {
                        let names = Box::new([c_path(&from)?, c_path(&to)?]);
                        let mut dirs = vec![open_dir_of(&to)?];
                        if from.parent() != to.parent() {
                            dirs.push(open_dir_of(&from)?);
                        }
                        self.start(tag, Op::Rename { names, dirs, chain: Chain::default() });
                        Ok(None)
                    })
                }
                Effect::Mode { file, mode, .. } => {
                    crate::fs::set_mode(&at(&file), mode).map(|()| Some(Done::Moded))
                }
                Effect::Remove { file, .. } => {
                    self.forget(&file);
                    crate::fs::remove(&at(&file)).map(|()| Some(Done::Removed))
                }
                Effect::Watch { dir, .. } => {
                    crate::watch::Tree::new(&self.root, &dir, true).map(|tree| {
                        let buffer = vec![0; crate::watch::BUFFER];
                        let id = self.start(tag, Op::Watch { tree, buffer, stopping: false });
                        self.watches.insert(tag, id);
                        None
                    })
                }
                Effect::Unwatch { .. } | Effect::Unlisten { .. } | Effect::Kill { .. } => {
                    unreachable!("taken above")
                }
                Effect::ReadAll { file, .. } => match self.file(&file, false) {
                    Ok(file) => size_of_file(file.fd()).map(|size| {
                        let bytes = vec![0; size as usize];
                        match size {
                            0 => Some(Done::Read(bytes)),
                            _ => {
                                self.start(tag, Op::Read { file, at: 0, bytes, got: 0 });
                                None
                            }
                        }
                    }),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        Ok(Some(Done::Read(Vec::new())))
                    }
                    Err(error) => Err(error),
                },
                Effect::Read { file, at, len, .. } => self.file(&file, false).map(|file| {
                    self.start(tag, Op::Read { file, at, bytes: vec![0; len], got: 0 });
                    None
                }),
                Effect::Append { file, bytes, .. } => {
                    self.file(&file, true).map(|file| match bytes.is_empty() {
                        true => Some(Done::Appended),
                        false => {
                            self.start(tag, Op::Append { file, bytes, put: 0 });
                            None
                        }
                    })
                }
                Effect::Sync { file, .. } => self.file(&file, true).map(|file| {
                    self.start(tag, Op::Sync { file });
                    None
                }),
                Effect::Truncate { file, len, .. } => self.file(&file, true).and_then(|file| {
                    // SAFETY: an open descriptor.
                    match unsafe { libc::ftruncate(file.fd(), len as libc::off_t) } {
                        0 => Ok(Some(Done::Truncated)),
                        _ => Err(io::Error::last_os_error()),
                    }
                }),
                Effect::Timer { after, .. } => {
                    let due = wheel::ticks(self.start.elapsed()).saturating_add(wheel::ticks(after));
                    self.timers.insert(due, tag);
                    Ok(None)
                }
                Effect::Map { file, .. } => crate::fs::map(&at(&file)).map(|it| Some(Done::Mapped(it))),
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
                        let op = Op::Connect { socket: Some(socket), at: Box::new(address) };
                        self.start(tag, op);
                        Ok(None)
                    }),
                Effect::Recv { conn, most, .. } => self.recv(conn, tag, most).map(|()| None),
                Effect::Send { conn, bytes, .. } if bytes.is_empty() => self
                    .conns
                    .get(&conn)
                    .filter(|it| !it.hung)
                    .map(|_| Some(Done::Sent))
                    .ok_or_else(|| gone(conn)),
                Effect::Send { conn, bytes, .. } => self.use_conn(conn).map(|socket| {
                    self.start(tag, Op::Send { conn, socket, bytes, put: 0 });
                    None
                }),
                Effect::Hangup { conn, .. } => self.hang_up(conn, tag).map(|()| None),
            };
            match started {
                Ok(None) => {}
                Ok(Some(done)) => self.ready.push(Completion { tag, done }),
                Err(error) => self
                    .ready
                    .push(Completion { tag, done: Done::Failed(IoError(error.to_string())) }),
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
        // A watch goes on after a change, and a listen after an accept.
        let ended = out[before..]
            .iter()
            .filter(|it| !matches!(it.done, Done::Changed { .. } | Done::Accepted { .. }))
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
        Address, Confined, FileName, Sandbox, Traced, Untraceable, run_until_idle,
        threads::Threads,
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
        let script = |driver: &mut dyn Driver| {
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
        };
        let (ours, theirs) = (dir("uring"), dir("uring-threads"));
        let ring = script(&mut Uring::new(&ours).unwrap());
        let threads = script(&mut Threads::new(&theirs, 2));
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
    fn processes(driver: &mut dyn Driver) -> BTreeMap<u64, Done> {
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
    fn round_trip(driver: &mut dyn Driver, at: Address) -> BTreeMap<u64, Vec<String>> {
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

    fn maps(driver: &mut dyn Driver, root: &Path) -> BTreeMap<u64, String> {
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
        let pid = ring.spawns[&Tag(1)].pid;
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
        let slot = |driver: &Uring, it: &str| driver.files[&name(it)].slot;
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
}
