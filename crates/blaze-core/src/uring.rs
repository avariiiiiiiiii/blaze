//! A [`Driver`] on Linux's io_uring: one ring, shared with the kernel through memory, that the
//! thread which polls fills with requests and reaps completions from, with no thread of its own
//! and one system call for a batch either way. Reads, appends, syncs and timers are the ring's;
//! a file is opened once and kept open. A truncation, which comes only in recovery, is a call
//! made where it is submitted.
//!
//! A spawned process is watched through the ring too: a poll of its pidfd for its exit, its
//! output pipes read and its input written by the ring. A listen's accepts (one multishot
//! request for all of them), a connect, and a connection's receives and sends are the ring's;
//! mapping a file is a call made where it is submitted. A listing, which may take long, runs on
//! a thread of its own, which wakes the ring through an eventfd it reads.
//!
//! The ring is set up with the system calls themselves (`io_uring_setup`, `io_uring_enter` and
//! three `mmap`s), the layouts as `linux/io_uring.h` has them, for one issuing thread whose
//! waits run the kernel's completion work (`IORING_SETUP_DEFER_TASKRUN`). Kernel 6.1 or later.

use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
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

use crate::{Command, Completion, Confine, Conn, Done, Driver, Effect, FileName, IoError, Tag};

const OP_FSYNC: u8 = 3;
const OP_WRITE_FIXED: u8 = 5;
const OP_POLL_ADD: u8 = 6;
const OP_TIMEOUT: u8 = 11;
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
/// A write at an offset of at least this many bytes, aligned to its file system's blocks,
/// bypasses the page cache (`O_DIRECT`).
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
    // SAFETY: a new private mapping of no file.
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
        // SAFETY: a limit to write.
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
        // SAFETY: the buffer the completion names, written by the kernel and not given back.
        unsafe { std::slice::from_raw_parts(self.buffers.at.add(bid as usize * RECV_BUFFER), len.min(RECV_BUFFER)) }
    }

    /// Buffer `bid` the kernel's again.
    fn give(&mut self, bid: u16) {
        let mask = RECV_BUFFERS - 1;
        let entry: *mut ProvidedBuf = self.ring.at((self.tail & mask) as u32 * size_of::<ProvidedBuf>() as u32);
        // SAFETY: an entry of the ring past the tail, which the kernel does not read until the
        // tail moves past it; the buffer is the driver's until then.
        unsafe {
            (*entry).addr = self.buffers.at.add(bid as usize * RECV_BUFFER) as u64;
            (*entry).len = RECV_BUFFER as u32;
            (*entry).bid = bid;
        }
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
        // SAFETY: within the mapping, as the debug assertion checks.
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
            // SAFETY: the array has `sq_entries` slots; each names the request of its index.
            unsafe { array.add(index as usize).write(index) };
        }
        // SAFETY: the kernel put these counters at these offsets of the mapping.
        let (sq_mask, cq_mask) =
            unsafe { (*rings.at::<u32>(sq.ring_mask), *rings.at::<u32>(cq.ring_mask)) };
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
        // SAFETY: the counters are the kernel's and this thread's; the kernel moves the head.
        let (head, tail) = unsafe {
            ((*self.sq_head).load(Ordering::Acquire), (*self.sq_tail).load(Ordering::Relaxed))
        };
        if tail.wrapping_sub(head) as usize + sqes.len() > self.sq_entries as usize {
            return false;
        }
        for (n, sqe) in sqes.iter().enumerate() {
            let at = tail.wrapping_add(n as u32) & self.sq_mask;
            // SAFETY: the slots from `tail` are not the kernel's until the tail moves past them.
            unsafe { self.sqes.add(at as usize).write(*sqe) };
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
        let wake = self.unsubmitted > 0 && flags & SQ_NEED_WAKEUP != 0;
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
        // SAFETY: the counters are the kernel's and this thread's; the kernel moves the tail.
        let (mut head, tail) = unsafe {
            ((*self.cq_head).load(Ordering::Relaxed), (*self.cq_tail).load(Ordering::Acquire))
        };
        while head != tail {
            // SAFETY: the entries between the head and the tail are posted and not reaped.
            let cqe = unsafe { &*self.cqes.add((head & self.cq_mask) as usize) };
            let (user_data, res, flags) = (cqe.user_data, cqe.res, cqe.flags);
            head = head.wrapping_add(1);
            // SAFETY: as above; the entry is the kernel's again once the head is past it.
            unsafe { (*self.cq_head).store(head, Ordering::Release) };
            each(user_data, res, flags);
        }
    }
}

/// A request in flight. Every buffer or address the kernel is given is on the heap and stays
/// where it is until the request's completion is reaped: moving an `Op` moves none of them.
enum Op {
    /// Into `bytes` from `at`, `got` of `bytes.len()` read so far.
    Read {
        fd: RawFd,
        at: u64,
        bytes: Vec<u8>,
        got: usize,
    },
    /// `bytes` at the end of the file, `put` of them written so far.
    Append {
        fd: RawFd,
        bytes: Vec<u8>,
        put: usize,
    },
    /// `bytes` at `at` of a file, through a descriptor of its own: the one kept for the file
    /// appends wherever it is told to write.
    WriteAt {
        fd: OwnedFd,
        at: u64,
        bytes: Vec<u8>,
        put: usize,
    },
    Sync {
        fd: RawFd,
    },
    /// The kernel reads the time from here until it completes.
    Timer {
        after: Box<Timespec>,
    },
    /// A whole file written to `temporary`, then synced, then renamed over `path` where it
    /// completes.
    Write {
        fd: OwnedFd,
        bytes: Vec<u8>,
        put: usize,
        synced: bool,
        temporary: PathBuf,
        path: PathBuf,
        file: FileName,
    },
    /// A read of a watch's events, asked again after each until the watch is stopped.
    Watch {
        tree: crate::watch::Tree,
        buffer: Vec<u8>,
        stopping: bool,
    },
    /// Stops the request `target`; its own completion is nobody's.
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
    /// An accept of a listen's, asked again after each until the listen is stopped.
    Accept {
        listener: crate::net::Listener,
        stopping: bool,
    },
    /// The kernel reads the address from `at` until it completes.
    Connect {
        fd: Option<OwnedFd>,
        at: Box<crate::net::SockAddr>,
    },
    /// A receive on `conn`, whose socket stays open while this is in flight.
    Recv {
        conn: Conn,
        fd: RawFd,
        bytes: Vec<u8>,
    },
    /// A send on `conn`, `put` of `bytes` sent so far.
    Send {
        conn: Conn,
        fd: RawFd,
        bytes: Vec<u8>,
        put: usize,
    },
    /// A read of the eventfd that threads doing work off the ring bump once they have sent
    /// its completion; in flight while any such work is.
    Wake {
        fd: RawFd,
        count: Box<[u8; 8]>,
    },
}

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
    socket: crate::net::Socket,
    /// Receives and sends of it in flight: its socket closes once none is and it is hung up.
    users: usize,
    hung: bool,
}

pub struct Uring {
    start: Instant,
    root: PathBuf,
    ring: Ring,
    /// Each file opened, for reading and appending.
    files: BTreeMap<FileName, OwnedFd>,
    /// By the `user_data` the ring hands back, which is never used for another request.
    pending: FxHashMap<u64, Pending>,
    next: u64,
    /// Requests for which the ring had no room.
    waiting: VecDeque<u64>,
    /// Completions to hand out at the next poll, in order.
    ready: Vec<Completion>,
    /// The request reading each watch's events, by the watch's tag.
    watches: BTreeMap<Tag, u64>,
    /// The request accepting each listen's connections, by the listen's tag.
    listens: BTreeMap<Tag, u64>,
    spawns: BTreeMap<Tag, Spawning>,
    conns: BTreeMap<Conn, Connection>,
    next_conn: u64,
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
        Ok(Uring {
            start: Instant::now(),
            root: root.into(),
            ring: Ring::new(ENTRIES, COMPLETIONS)?,
            files: BTreeMap::new(),
            pending: FxHashMap::default(),
            next: 0,
            waiting: VecDeque::new(),
            ready: Vec::new(),
            watches: BTreeMap::new(),
            listens: BTreeMap::new(),
            spawns: BTreeMap::new(),
            conns: BTreeMap::new(),
            next_conn: 0,
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

    /// Forgets the descriptor kept for `name`: the name now is another file.
    fn forget(&mut self, name: &FileName) {
        self.files.remove(name);
    }

    /// The file, opened once; `make` makes it if missing, with its directories.
    fn file(&mut self, name: &FileName, make: bool) -> io::Result<RawFd> {
        if let Some(fd) = self.files.get(name) {
            return Ok(fd.as_raw_fd());
        }
        let path = self.root.join(name.as_str());
        if make && let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let fd = open(&path, make)?;
        let raw = fd.as_raw_fd();
        self.files.insert(name.clone(), fd);
        Ok(raw)
    }

    fn start(&mut self, tag: Tag, op: Op) -> u64 {
        let id = self.next;
        self.next += 1;
        self.pending.insert(id, Pending { tag, op });
        self.queue(id);
        id
    }

    /// Writes the request `id` as it now stands to the ring, or keeps it for when there is
    /// room.
    fn queue(&mut self, id: u64) {
        if !self.waiting.is_empty() || !self.ring.push(self.sqe(id)) {
            self.waiting.push_back(id);
        }
    }

    fn sqe(&self, id: u64) -> Sqe {
        let pending = &self.pending[&id];
        let user_data = id;
        // SAFETY of the kernel's reads and writes: every buffer and address below stays where
        // it is until the request's completion (see `Op`).
        match &pending.op {
            Op::Read { fd, at, bytes, got } => Sqe {
                opcode: OP_READ,
                fd: *fd,
                off: at + *got as u64,
                addr: bytes[*got..].as_ptr() as u64,
                len: (bytes.len() - got) as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Append { fd, bytes, put } => Sqe {
                opcode: OP_WRITE,
                fd: *fd,
                // The file's position, which is its end: it is open for appending.
                off: u64::MAX,
                addr: bytes[*put..].as_ptr() as u64,
                len: (bytes.len() - put) as u32,
                user_data,
                ..Sqe::default()
            },
            Op::WriteAt { fd, at, bytes, put } => Sqe {
                opcode: OP_WRITE,
                fd: fd.as_raw_fd(),
                off: at + *put as u64,
                addr: bytes[*put..].as_ptr() as u64,
                len: (bytes.len() - put).min(u32::MAX as usize) as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Sync { fd } => Sqe { opcode: OP_FSYNC, fd: *fd, user_data, ..Sqe::default() },
            Op::Timer { after } => Sqe {
                opcode: OP_TIMEOUT,
                fd: -1,
                addr: &**after as *const Timespec as u64,
                len: 1,
                user_data,
                ..Sqe::default()
            },
            Op::Write { fd, synced: false, bytes, put, .. } if *put < bytes.len() => Sqe {
                opcode: OP_WRITE,
                fd: fd.as_raw_fd(),
                off: *put as u64,
                addr: bytes[*put..].as_ptr() as u64,
                len: (bytes.len() - put) as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Write { fd, .. } => {
                Sqe { opcode: OP_FSYNC, fd: fd.as_raw_fd(), user_data, ..Sqe::default() }
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
                user_data,
                ..Sqe::default()
            },
            Op::Connect { fd, at } => Sqe {
                opcode: OP_CONNECT,
                fd: fd.as_ref().map_or(-1, |it| it.as_raw_fd()),
                addr: &raw const at.storage as u64,
                // The address's length.
                off: u64::from(at.len),
                user_data,
                ..Sqe::default()
            },
            Op::Recv { fd, bytes, .. } => Sqe {
                opcode: OP_RECV,
                fd: *fd,
                addr: bytes.as_ptr() as u64,
                len: bytes.len().min(u32::MAX as usize) as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Send { fd, bytes, put, .. } => Sqe {
                opcode: OP_SEND,
                fd: *fd,
                addr: bytes[*put..].as_ptr() as u64,
                len: (bytes.len() - put).min(u32::MAX as usize) as u32,
                op_flags: libc::MSG_NOSIGNAL as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Wake { fd, count } => Sqe {
                opcode: OP_READ,
                fd: *fd,
                off: u64::MAX,
                addr: count.as_ptr() as u64,
                len: count.len() as u32,
                user_data,
                ..Sqe::default()
            },
        }
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
        while let Some(&id) = self.waiting.front() {
            if !self.pending.contains_key(&id) {
                self.waiting.pop_front();
                continue;
            }
            let sqe = self.sqe(id);
            if !self.ring.push(sqe) {
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
                // thread.
                Op::Cancel { .. }
                | Op::Exit { .. }
                | Op::Output { .. }
                | Op::Input { .. }
                | Op::Wake { .. } => {}
                _ => self.ready.push(failed(pending.tag)),
            }
            self.abandoned.push(pending);
        }
        for (tag, mut spawn) in std::mem::take(&mut self.spawns) {
            if !spawn.exited {
                _ = kill(spawn.pidfd.as_raw_fd());
            }
            spawn.failed = Some(why.clone());
            self.ready.push(Completion { tag, done: spawn.end() });
        }
        self.conns.retain(|_, conn| {
            conn.users = 0;
            !conn.hung
        });
        self.watches.clear();
        self.listens.clear();
        self.offload.reading = None;
        self.waiting.clear();
        self.ring.unsubmitted = 0;
    }

    /// Takes the completion of the request `id`: its effect's completion, or the rest of it
    /// asked for. With `more`, the kernel still holds the request (a multishot one), so it is
    /// kept and not asked again.
    fn completed(&mut self, id: u64, res: i32, more: bool) {
        let Some(mut pending) = self.pending.remove(&id) else { return };
        let tag = pending.tag;
        match self.step(tag, &mut pending.op, res, more) {
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
                    Op::Recv { conn, .. } | Op::Send { conn, .. } => self.release(conn),
                    _ => {}
                }
            }
        }
    }

    /// What the result `res` of a request of the effect `tag` means for it.
    fn step(&mut self, tag: Tag, op: &mut Op, res: i32, more: bool) -> Step {
        let error = |res: i32| io::Error::from_raw_os_error(-res).to_string();
        let failed = |res: i32| Some(Done::Failed(IoError(error(res))));
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
            Op::Timer { .. } if res == -libc::ETIME || res == 0 => Step::Ends(Some(Done::Fired)),
            Op::Timer { .. } => Step::Ends(failed(res)),
            Op::Write { temporary, .. } if res < 0 => {
                _ = std::fs::remove_file(&*temporary);
                Step::Ends(failed(res))
            }
            Op::Write { synced: false, bytes, put, temporary, .. } if *put < bytes.len() => {
                if res == 0 {
                    _ = std::fs::remove_file(&*temporary);
                    return Step::Ends(failed(-libc::EIO));
                }
                *put += res as usize;
                // The rest of it, or the sync once it is all written.
                Step::Again
            }
            Op::Write { synced, temporary, path, file, .. } => {
                *synced = true;
                let file = file.clone();
                self.forget(&file);
                Step::Ends(Some(match crate::fs::commit_rename(temporary, path) {
                    Ok(()) => Done::Written,
                    Err(error) => Done::Failed(IoError(error.to_string())),
                }))
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
            Op::Accept { listener, stopping } => {
                if res >= 0 {
                    // SAFETY: the accept made this descriptor, owned from here.
                    let fd = unsafe { OwnedFd::from_raw_fd(res) };
                    let conn = self.add(crate::net::Socket::from_fd(fd, listener.is_unix()));
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
                    return Step::Again;
                }
                if res == -libc::ECANCELED || *stopping {
                    self.listens.remove(&tag);
                    return Step::Ends(Some(Done::Unlistened));
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
            Op::Connect { fd, at } => {
                if res < 0 {
                    return Step::Ends(failed(res));
                }
                let fd = fd.take().expect("a connect's socket");
                let conn = self.add(crate::net::Socket::from_fd(fd, at.domain == libc::AF_UNIX));
                Step::Ends(Some(Done::Connected { conn }))
            }
            Op::Recv { bytes, .. } => {
                if res == -libc::ECONNRESET {
                    // The peer reset it: as good as hung up, for a reader.
                    return Step::Ends(Some(Done::Read(Vec::new())));
                }
                if res < 0 {
                    return Step::Ends(failed(res));
                }
                let mut bytes = std::mem::take(bytes);
                bytes.truncate(res as usize);
                Step::Ends(Some(Done::Read(bytes)))
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
        }
    }

    fn reap(&mut self) {
        self.take_offloaded();
        let mut posted = Vec::new();
        self.ring.reap(|user_data, res, more| posted.push((user_data, res, more)));
        for (id, res, more) in posted {
            self.completed(id, res, more);
        }
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
            self.start(tag, Op::Cancel { target: id });
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

    fn add(&mut self, socket: crate::net::Socket) -> Conn {
        if let crate::net::Socket::Tcp(stream) = &socket {
            // Latency only: a request and its answer are each one send.
            _ = stream.set_nodelay(true);
        }
        let conn = Conn(self.next_conn);
        self.next_conn += 1;
        self.conns.insert(conn, Connection { socket, users: 0, hung: false });
        conn
    }

    /// The socket of `conn`, kept open until [`Uring::release`] says the request using it
    /// ended.
    fn use_conn(&mut self, conn: Conn) -> io::Result<RawFd> {
        let connection = self.conns.get_mut(&conn).filter(|it| !it.hung).ok_or_else(|| gone(conn))?;
        connection.users += 1;
        Ok(connection.socket.fd())
    }

    fn release(&mut self, conn: Conn) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        connection.users -= 1;
        if connection.hung && connection.users == 0 {
            self.conns.remove(&conn);
        }
    }

    /// Shuts `conn` both ways, so a receive in flight on it ends empty, and closes it once
    /// nothing in flight uses it.
    fn hang_up(&mut self, conn: Conn) -> io::Result<()> {
        let connection = self.conns.get_mut(&conn).filter(|it| !it.hung).ok_or_else(|| gone(conn))?;
        connection.hung = true;
        let shut = connection.socket.shutdown();
        if connection.users == 0 {
            self.conns.remove(&conn);
        }
        shut
    }
}

impl Drop for Uring {
    /// Kills the processes of spawns in flight and takes back from the kernel every buffer
    /// it was lent (each request cancelled, its completion waited for), so none is freed while
    /// the kernel may still write into it. What it does not give back in time is leaked.
    fn drop(&mut self) {
        for spawn in self.spawns.values() {
            if !spawn.exited {
                _ = kill(spawn.pidfd.as_raw_fd());
            }
        }
        // Never handed to the kernel.
        for id in std::mem::take(&mut self.waiting) {
            self.pending.remove(&id);
        }
        let deadline = Instant::now() + LETTING_GO;
        let mut posted = Vec::new();
        let targets: Vec<u64> = self.pending.keys().copied().collect();
        let mut alive = true;
        for target in targets {
            let cancel = Sqe {
                opcode: OP_ASYNC_CANCEL,
                fd: -1,
                addr: target,
                user_data: NOBODY,
                ..Sqe::default()
            };
            while alive && !self.ring.push(cancel) {
                alive = Instant::now() < deadline && self.ring.enter(None).is_ok();
                self.ring.reap(|user_data, res, more| posted.push((user_data, res, more)));
            }
        }
        while alive {
            for (id, res, more) in posted.drain(..) {
                if res >= 0
                    && let Some(Pending { op: Op::Accept { .. }, .. }) = self.pending.get(&id)
                {
                    // SAFETY: a connection the accept made after it was cancelled, owned here
                    // and closed.
                    drop(unsafe { OwnedFd::from_raw_fd(res) });
                }
                // A multishot request the kernel still holds is not given back yet.
                if !more {
                    self.pending.remove(&id);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let lent = self.pending.values().any(|it| !matches!(it.op, Op::Cancel { .. }));
            if !lent || left.is_zero() {
                break;
            }
            alive = self.ring.enter(Some(left)).is_ok();
            self.ring.reap(|user_data, res, more| posted.push((user_data, res, more)));
        }
        for (_, spawn) in std::mem::take(&mut self.spawns) {
            _ = spawn.end();
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

fn open(path: &Path, make: bool) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
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
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
    let flags = libc::O_WRONLY | libc::O_CLOEXEC | flags;
    // SAFETY: a NUL-terminated path; the descriptor returned is this driver's.
    let fd = unsafe { libc::open(path.as_ptr(), flags, 0o644 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn size_of_file(fd: RawFd) -> io::Result<u64> {
    // SAFETY: `stat` is written by the call.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: an open descriptor and a `stat` to write.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.st_size as u64)
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
                    crate::fs::make_parent(&path).and_then(|()| create(&temporary)).map(|fd| {
                        self.start(tag, Op::Write {
                            fd,
                            bytes,
                            put: 0,
                            synced: false,
                            temporary,
                            path,
                            file,
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
                    open_to_write(&at(&file), 0).map(|fd| match bytes.is_empty() {
                        true => Some(Done::Written),
                        false => {
                            self.start(tag, Op::WriteAt { fd, at: offset, bytes, put: 0 });
                            None
                        }
                    })
                }
                Effect::Rename { from, to, .. } => {
                    self.forget(&from);
                    self.forget(&to);
                    crate::fs::rename(&at(&from), &at(&to)).map(|()| Some(Done::Renamed))
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
                    Ok(fd) => size_of_file(fd).map(|size| {
                        let bytes = vec![0; size as usize];
                        match size {
                            0 => Some(Done::Read(bytes)),
                            _ => {
                                self.start(tag, Op::Read { fd, at: 0, bytes, got: 0 });
                                None
                            }
                        }
                    }),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        Ok(Some(Done::Read(Vec::new())))
                    }
                    Err(error) => Err(error),
                },
                Effect::Read { file, at, len, .. } => self.file(&file, false).map(|fd| {
                    self.start(tag, Op::Read { fd, at, bytes: vec![0; len], got: 0 });
                    None
                }),
                Effect::Append { file, bytes, .. } => {
                    self.file(&file, true).map(|fd| match bytes.is_empty() {
                        true => Some(Done::Appended),
                        false => {
                            self.start(tag, Op::Append { fd, bytes, put: 0 });
                            None
                        }
                    })
                }
                Effect::Sync { file, .. } => self.file(&file, true).map(|fd| {
                    self.start(tag, Op::Sync { fd });
                    None
                }),
                Effect::Truncate { file, len, .. } => self.file(&file, true).and_then(|fd| {
                    // SAFETY: an open descriptor.
                    match unsafe { libc::ftruncate(fd, len as libc::off_t) } {
                        0 => Ok(Some(Done::Truncated)),
                        _ => Err(io::Error::last_os_error()),
                    }
                }),
                Effect::Timer { after, .. } => {
                    let after = Box::new(Timespec {
                        sec: after.as_secs() as i64,
                        nsec: after.subsec_nanos() as i64,
                    });
                    self.start(tag, Op::Timer { after });
                    Ok(None)
                }
                Effect::Map { file, .. } => crate::fs::map(&at(&file)).map(|it| Some(Done::Mapped(it))),
                Effect::Spawn { command, .. } => self.spawn(tag, command).map(|()| None),
                Effect::Listen { at: address, .. } => {
                    crate::net::listen(&self.root, &address).map(|listener| {
                        let id = self.start(tag, Op::Accept { listener, stopping: false });
                        self.listens.insert(tag, id);
                        None
                    })
                }
                Effect::Connect { at: address, .. } => crate::net::sockaddr(&self.root, &address)
                    .and_then(|address| {
                        let fd = crate::net::stream_socket(address.domain)?;
                        self.start(tag, Op::Connect { fd: Some(fd), at: Box::new(address) });
                        Ok(None)
                    }),
                Effect::Recv { conn, most, .. } => self.use_conn(conn).map(|fd| {
                    self.start(tag, Op::Recv { conn, fd, bytes: vec![0; most] });
                    None
                }),
                Effect::Send { conn, bytes, .. } if bytes.is_empty() => self
                    .conns
                    .get(&conn)
                    .filter(|it| !it.hung)
                    .map(|_| Some(Done::Sent))
                    .ok_or_else(|| gone(conn)),
                Effect::Send { conn, bytes, .. } => self.use_conn(conn).map(|fd| {
                    self.start(tag, Op::Send { conn, fd, bytes, put: 0 });
                    None
                }),
                Effect::Hangup { conn, .. } => self.hang_up(conn).map(|()| Some(Done::HungUp)),
            };
            match started {
                Ok(None) => {}
                Ok(Some(done)) => self.ready.push(Completion { tag, done }),
                Err(error) => self
                    .ready
                    .push(Completion { tag, done: Done::Failed(IoError(error.to_string())) }),
            }
        }
        self.submit_ring();
    }

    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize {
        let before = out.len();
        self.reap();
        out.append(&mut self.ready);
        if out.len() == before && self.in_flight > 0 {
            let deadline = Instant::now() + wait;
            while out.len() == before {
                let left = deadline.saturating_duration_since(Instant::now());
                if let Err(error) = self.ring.enter(Some(left)) {
                    self.fail_unsubmitted(error);
                    out.append(&mut self.ready);
                    break;
                }
                self.reap();
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
}
