//! The driver on Windows: one I/O completion port that everything ends on.
//!
//! Reads, appends and writes at offsets go to the port as overlapped requests on handles of
//! the driver's (`FILE_FLAG_OVERLAPPED`), as do a watch's `ReadDirectoryChangesW`, a process's
//! pipes and every socket call (`AcceptEx`, `ConnectEx`, `WSARecv`, `WSASend`). The calls
//! Windows has no overlapped form of (a whole-file write, a rename, a clone, a stat, a listing,
//! a sync's `FlushFileBuffers`, a mapping) run on the driver's threads, which post to the port
//! when one is done. A process's end is posted by a wait of the thread pool's, its job's
//! messages by the job; timers are the port's wait, ended at the next one due.
//!
//! A journal's reads of the volume's USN journal are on the port too ([`usn`]); what each read
//! names is made on the driver's threads, which ask the volume where files are, and the next
//! read is asked for once they hand the journal back. Stamps and listings run on the threads.
//!
//! The rule every request keeps (see [`Request`]): what the kernel reads or writes for it (its
//! `OVERLAPPED`, its bytes, the handle it is on) is the request's, in its entry of the driver's
//! slab, which is let go only once its completion is taken from the port, never before. A
//! packet names its request by the bits of the request's handle, so one naming a request that
//! has ended (its slot holding another since) is stale: dropped and counted, never read
//! through.
//!
//! Everything that comes and goes is in a slab named by generational handles: requests, the
//! handles files are read and written through, processes and connections. A [`Conn`] is its
//! connection's handle's bits, and a process's exit packet and its job's key carry its handle's.
//! The buffers the kernel writes into are [`Buffers`], so a warm cycle allocates only what its
//! completion carries.

pub(crate) mod files;
mod net;
mod process;
mod stamp;
mod sys;
mod usn;
mod watch;

pub(crate) use self::stamp::{entries, stamps};

use std::{
    cell::UnsafeCell,
    collections::VecDeque,
    fmt, io,
    mem::ManuallyDrop,
    os::windows::io::{AsRawHandle as _, OwnedHandle, OwnedSocket},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use blaze_mem::{Slab, Tag as MemTag};
use crossbeam_channel::{Receiver, Sender};
use rustc_hash::FxHashMap;
use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_CONNECTION_ABORTED, ERROR_HANDLE_EOF, ERROR_NETNAME_DELETED,
        ERROR_NO_DATA, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, HANDLE,
    },
    Networking::WinSock::{SOCKET, WSAECONNRESET},
    Storage::FileSystem::{ReadFile, WriteFile},
    System::{
        IO::{CancelIoEx, OVERLAPPED_ENTRY},
        SystemServices::JOB_OBJECT_MSG_JOB_MEMORY_LIMIT,
        Threading::WaitForSingleObject,
    },
};

use self::sys::{BUFFER, Buffer, Buffers, Head, KEY_EXIT, KEY_IO, KEY_JOB, KEY_WAKE, Port};
use crate::{
    Address, Command, Completion, Confine, Conn, Cursor, Done, Driver, Effect, FileName, IoError, Tag,
    wheel::{self, Wheel},
};

/// Packets taken from the port at once.
const ENTRIES: usize = 64;
/// Handles kept open for reading and appending, by name; past this many they are all let go.
const CACHED: usize = 256;
/// How long dropping the driver waits for the port to give back what it was lent.
const GIVE_BACK: Duration = Duration::from_secs(5);
/// Requests in flight at once: the room the slab of them reserves.
const REQUESTS: u32 = 1 << 18;
/// Handles of files open at once: the cached ones and those of the writes at offsets in flight.
const FILES: u32 = 1 << 16;
/// Processes followed at once.
const SPAWNS: u32 = 1 << 14;
/// Connections open at once.
const CONNS: u32 = 1 << 16;
/// Buffers the kernel writes into at once: a watch's and a process's two outputs'.
const BUFFERS: u32 = 1 << 12;
/// Work at the driver's threads at once, and so the most of what they send back that waits;
/// more waits in the driver's backlog.
const WORK: usize = 1024;
/// Watches, journals, listens and processes by tag that the maps of them have room for from the
/// start.
const TAGGED: usize = 1024;
/// Completions made on the polling thread that wait for a poll without growing the list.
const READY: usize = 4 * ENTRIES;

type Id = blaze_mem::Handle<Request>;
type FileId = blaze_mem::Handle<Cached>;
type SpawnId = blaze_mem::Handle<Spawned>;
type ConnId = blaze_mem::Handle<Connection>;

/// What a request in flight is doing.
enum Op {
    /// `want` bytes from `at` into the spare room of `bytes`, which the kernel writes and the
    /// completion carries as it is: all of them, or to the end of the file where `to_end`,
    /// `want` growing. On `file`, whose handle `raw` is open while the request is on it.
    Read { file: FileId, raw: HANDLE, bytes: Vec<u8>, at: u64, want: usize, to_end: bool },
    /// `bytes` written at `at`, or at the end where `None` (an append).
    Write { file: FileId, raw: HANDLE, bytes: Vec<u8>, at: Option<u64>, put: usize },
    Watch(watch::Watching),
    /// A read of the USN journal.
    Journal(Box<usn::Journal>),
    /// A process's output, `err` its standard error, read into `buffer`.
    Output { spawn: SpawnId, err: bool, pipe: OwnedHandle, buffer: Buffer },
    Input { spawn: SpawnId, pipe: OwnedHandle, bytes: Vec<u8>, put: usize },
    Accept(net::Accepting),
    Connect { connecting: net::Connecting, at: Address },
    /// Into the spare room of `bytes` (as much as was asked for, up to [`BUFFER`]), which the
    /// kernel writes and the completion carries as it is; `flags` are Winsock's to write as it
    /// completes. On the socket of `conn`, which stays open while the request is on it.
    Recv { conn: ConnId, socket: SOCKET, bytes: Vec<u8>, flags: UnsafeCell<u32> },
    Send { conn: ConnId, socket: SOCKET, bytes: Vec<u8>, put: usize },
}

impl Op {
    /// What the request is on: what cancelling it names.
    fn handle(&self) -> HANDLE {
        match self {
            Op::Read { raw, .. } | Op::Write { raw, .. } => *raw,
            Op::Watch(watching) => watching.raw(),
            Op::Journal(journal) => journal.raw(),
            Op::Output { pipe, .. } | Op::Input { pipe, .. } => pipe.as_raw_handle(),
            Op::Accept(accepting) => net::raw(&accepting.listener.socket) as HANDLE,
            Op::Connect { connecting, .. } => connecting.raw() as HANDLE,
            Op::Recv { socket, .. } | Op::Send { socket, .. } => *socket as HANDLE,
        }
    }
}

/// An overlapped request: its head, whose `OVERLAPPED` the kernel holds while a step is in
/// flight, and its op, which owns the bytes and what it is on. It lives in its entry of
/// [`Iocp::requests`], which never moves while it is there, and every step is asked for in
/// place; the entry is let go only once the completion of the last step is taken.
struct Request {
    tag: Tag,
    head: Head,
    op: Op,
}

impl Request {
    /// Asks the kernel for the op's next step. Ok: its completion comes to the port.
    fn ask(&mut self) -> io::Result<()> {
        let Request { head, op, .. } = self;
        match op {
            Op::Read { raw, bytes, at, want, .. } => {
                let filled = bytes.len();
                let rest = (*want - filled).min(bytes.capacity() - filled);
                let into = bytes.spare_capacity_mut().as_mut_ptr().cast::<u8>();
                let overlapped = head.reset(*at + filled as u64);
                // SAFETY: an open overlapped handle (the file's, open while the request is on
                // it), the `Vec`'s spare room of at least `rest` bytes, which the kernel only
                // writes, and an `OVERLAPPED`, both in the request's entry, which is kept until
                // the completion is taken (see `Request`).
                sys::pending(unsafe { ReadFile(*raw, into, len(rest), std::ptr::null_mut(), overlapped) })
            }
            Op::Write { raw, bytes, at, put, .. } => {
                let rest = &bytes[*put..];
                // All ones: at the end of the file, wherever that is when it is written.
                let overlapped = head.reset(at.map_or(u64::MAX, |at| at + *put as u64));
                // SAFETY: as for a read; the bytes are only read.
                sys::pending(unsafe { WriteFile(*raw, rest.as_ptr(), len(rest.len()), std::ptr::null_mut(), overlapped) })
            }
            Op::Watch(watching) => watching.read(head.reset(0)),
            Op::Journal(journal) => journal.read(head.reset(0)),
            Op::Output { pipe, buffer, .. } => {
                let overlapped = head.reset(0);
                // SAFETY: as for a read; the buffer is the request's until it ends (`let_go`).
                sys::pending(unsafe {
                    ReadFile(pipe.as_raw_handle(), buffer.ptr(), BUFFER as u32, std::ptr::null_mut(), overlapped)
                })
            }
            Op::Input { pipe, bytes, put, .. } => {
                let rest = &bytes[*put..];
                let overlapped = head.reset(0);
                // SAFETY: as for a write.
                sys::pending(unsafe {
                    WriteFile(pipe.as_raw_handle(), rest.as_ptr(), len(rest.len()), std::ptr::null_mut(), overlapped)
                })
            }
            Op::Accept(accepting) => accepting.accept(head.reset(0)),
            Op::Recv { socket, bytes, flags, .. } => {
                net::recv(*socket, bytes.as_mut_ptr(), bytes.capacity(), flags.get(), head.reset(0))
            }
            Op::Send { socket, bytes, put, .. } => net::send(*socket, &bytes[*put..], head.reset(0)),
            Op::Connect { .. } => unreachable!("a connect is asked for once, as it is made"),
        }
    }

    /// Asks the kernel to end it early; its completion still comes, as cancelled or as it
    /// ended before.
    fn cancel(&self) {
        // SAFETY: the handle the request is on and its `OVERLAPPED`, both alive (the
        // request's). Failing (`ERROR_NOT_FOUND`) means it has ended already.
        unsafe { CancelIoEx(self.op.handle(), self.head.overlapped()) };
    }
}

/// A length as a call takes one: a step moves at most this much, and the next asks for the rest.
fn len(bytes: usize) -> u32 {
    bytes.min(u32::MAX as usize) as u32
}

/// What the end of a request's step comes to.
enum After {
    /// Its next step, asked for in place.
    Again,
    /// The request let go, then what follows of it.
    Over(Then),
}

enum Then {
    Nothing,
    /// The process may have ended.
    Spawn(SpawnId),
    /// The connection may take its next receive, or close.
    Recv(ConnId),
    /// Its next send, or close.
    Send(ConnId),
}

/// A handle the port's reads and writes are on: kept by its name (`readers`, `appenders`)
/// while `named`, and open while it is named or a request is on it.
struct Cached {
    handle: OwnedHandle,
    users: u32,
    named: bool,
}

/// A request on `id` ended.
fn release(files: &mut Slab<Cached>, id: FileId) {
    let Some(cached) = files.get_mut(id) else { return };
    cached.users -= 1;
    if cached.users == 0 && !cached.named {
        files.remove(id);
    }
}

/// `id` no longer kept by its name: closed once no request is on it.
fn unname(files: &mut Slab<Cached>, id: FileId) {
    let Some(cached) = files.get_mut(id) else { return };
    cached.named = false;
    if cached.users == 0 {
        files.remove(id);
    }
}

/// What the driver's threads send back.
enum Back {
    Done(Completion),
    /// A Unix socket's connect, made with a blocking call (see `net`).
    Connected { tag: Tag, at: Address, made: io::Result<OwnedSocket> },
    /// A journal opened, or a read of it named: the journal to read on with, unless it ended.
    Journal { tag: Tag, journal: Option<Box<usn::Journal>>, taken: usn::Taken },
}

/// What the driver's threads are given.
enum Work {
    File(Effect),
    Connect { tag: Tag, at: Address },
    /// A journal to open, its volume to be put on the port.
    Open { tag: Tag, dir: FileName, since: Cursor },
    /// What a read of a journal ended with, to be named.
    Took { tag: Tag, journal: Box<usn::Journal>, result: io::Result<u32> },
}

/// A journal's watch: whether its read is in flight (its request) or its journal is on the
/// driver's threads, and whether it was asked to end.
struct Journaling {
    request: Option<Id>,
    stopping: bool,
}

struct Spawned {
    tag: Tag,
    process: OwnedHandle,
    job: OwnedHandle,
    /// What the thread pool's wait reads as the process ends: in this entry, which stays where
    /// it is until `waiting` is dropped (`end_wait`).
    waiter: process::Waiter,
    waiting: Option<process::Waiting>,
    out: Vec<u8>,
    err: Vec<u8>,
    /// Of its two outputs, how many have not ended.
    outputs: u8,
    exited: bool,
    /// The request writing its input, while it is in flight.
    input: Option<Id>,
    /// Why its output or input could not all be moved.
    failed: Option<io::Error>,
    finish: process::Finish,
}

struct Connection {
    socket: OwnedSocket,
    recvs: VecDeque<(Tag, usize)>,
    sends: VecDeque<(Tag, Vec<u8>)>,
    /// One receive and one send in flight at a time, so their bytes keep their order.
    receiving: bool,
    sending: bool,
    /// Hung up: it names nothing to the caller, and closes once what is in flight on it ends.
    hung_up: bool,
}

pub struct Iocp {
    start: Instant,
    root: Arc<Path>,
    paths: sys::Paths,
    port: Arc<Port>,
    back: Receiver<Back>,
    /// Whether a thread has posted a wake that the driver has not yet drained `back` after:
    /// so a batch of what the threads send costs one packet, not one each.
    woken: Arc<AtomicBool>,
    work: Option<Sender<Work>>,
    /// Work asked for beyond the [`WORK`] at the threads, in the order asked.
    backlog: VecDeque<Work>,
    at_threads: usize,
    workers: Vec<JoinHandle<()>>,
    // dyn: the caller's sandbox maker, asked once per sandboxed process.
    confine: Option<Arc<dyn Confine>>,
    files: Slab<Cached>,
    readers: FxHashMap<FileName, FileId>,
    appenders: FxHashMap<FileName, FileId>,
    /// Every request in flight. Dropped only once none is (`Drop`): the kernel may write any
    /// of their entries until its packet is taken.
    requests: ManuallyDrop<Slab<Request>>,
    /// Packets dropped as naming a request that had ended.
    stale: u64,
    /// As `requests`, as the kernel writes into them.
    buffers: ManuallyDrop<Buffers>,
    scratch: watch::Scratch,
    /// Completions made on the polling thread, in the order they happened.
    ready: Vec<Completion>,
    /// By ticks since `start`: due, then the order they were asked for.
    timers: Wheel<Tag>,
    watches: FxHashMap<Tag, Id>,
    journals: FxHashMap<Tag, Journaling>,
    listens: FxHashMap<Tag, Id>,
    spawns: Slab<Spawned>,
    spawning: FxHashMap<Tag, SpawnId>,
    conns: Slab<Connection>,
    entries: [OVERLAPPED_ENTRY; ENTRIES],
    in_flight: usize,
}

// SAFETY: what is not `Send` in the driver is the raw handles and sockets its requests copy
// from what they are on and the buffers they point into, all of them the driver's own and none
// tied to the thread that made it; the driver is used from one thread at a time (`&mut self`).
unsafe impl Send for Iocp {}

fn reserved<K, V>() -> FxHashMap<K, V> {
    // alloc: setup, the room of a map of what is named by tag.
    FxHashMap::with_capacity_and_hasher(TAGGED, Default::default())
}

impl Iocp {
    /// A driver whose files are under `root`, with `threads` threads for the calls that block.
    pub fn new(root: &Path, threads: usize) -> io::Result<Iocp> {
        let root: Arc<Path> = root.into();
        // alloc: setup, the port the driver's threads post to.
        let port = Arc::new(Port::new()?);
        // Every work sends one thing back, and at most `WORK` are at the threads: neither
        // channel is ever full.
        let (sender, back) = crossbeam_channel::bounded(WORK);
        let (work, queue) = crossbeam_channel::bounded::<Work>(WORK);
        // alloc: setup, the flag the threads share.
        let woken = Arc::new(AtomicBool::new(false));
        let workers = (0..threads.max(1))
            .map(|n| {
                let (root, queue, port, sender) = (root.clone(), queue.clone(), port.clone(), sender.clone()); // clone: setup.
                let woken = woken.clone(); // clone: setup, what each thread is given.
                std::thread::Builder::new()
                    // alloc: setup.
                    .name(format!("blaze-iocp-{n}"))
                    .spawn(move || work_on(&root, &queue, &port, &sender, &woken))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mem = io::Error::other;
        Ok(Iocp {
            start: Instant::now(),
            paths: sys::Paths::new(&root),
            root,
            port,
            back,
            woken,
            work: Some(work),
            // alloc: setup.
            backlog: VecDeque::with_capacity(WORK),
            at_threads: 0,
            workers,
            confine: None,
            files: Slab::new(FILES, MemTag::Io).map_err(mem)?,
            // alloc: setup, room for every handle kept.
            readers: FxHashMap::with_capacity_and_hasher(CACHED, Default::default()),
            // alloc: as above.
            appenders: FxHashMap::with_capacity_and_hasher(CACHED, Default::default()),
            requests: ManuallyDrop::new(Slab::new(REQUESTS, MemTag::Io).map_err(mem)?),
            stale: 0,
            buffers: ManuallyDrop::new(Buffers::new(BUFFERS).map_err(mem)?),
            scratch: watch::Scratch::default(),
            // alloc: setup.
            ready: Vec::with_capacity(READY),
            timers: Wheel::try_new(wheel::MAX_TIMERS).map_err(mem)?,
            watches: reserved(),
            journals: reserved(),
            listens: reserved(),
            spawns: Slab::new(SPAWNS, MemTag::Io).map_err(mem)?,
            spawning: reserved(),
            conns: Slab::new(CONNS, MemTag::Io).map_err(mem)?,
            // SAFETY: all zeros is an entry of nothing, written over before it is read.
            entries: unsafe { std::mem::zeroed() },
            in_flight: 0,
        })
    }

    /// The driver, starting each process whose sandbox is on as `confine` makes it.
    // dyn: as the field.
    pub fn confined(mut self, confine: Arc<dyn Confine>) -> Iocp {
        self.confine = Some(confine);
        self
    }

    /// Where the TCP listen `listen` listens, for one asked of port 0.
    pub fn local_addr(&self, listen: Tag) -> Option<std::net::SocketAddr> {
        let id = self.listens.get(&listen)?;
        match &self.requests.get(*id)?.op {
            Op::Accept(accepting) => accepting.listener.local_addr(),
            _ => None,
        }
    }

    /// Packets the port handed back that named a request already ended, dropped.
    pub fn stale(&self) -> u64 {
        self.stale
    }

    /// A request of `op` for `tag`, in place in the slab and not yet asked for.
    fn request(&mut self, tag: Tag, op: Op) -> Id {
        self.requests.insert_with(|id| Request { tag, head: Head::new(id.to_bits()), op })
    }

    /// The request `id` asked for its next step, kept until its completion; or, refused at
    /// once, ended now.
    fn put(&mut self, id: Id) {
        let Some(request) = self.requests.get_mut(id) else { return };
        if let Err(error) = request.ask() {
            self.ended(id, Err(error));
        }
    }

    fn fail(&mut self, tag: Tag, why: impl fmt::Display) {
        self.ready.push(failed(tag, why));
    }

    /// `work` to the threads, or, where [`WORK`] are there already, to the backlog.
    fn to_threads(&mut self, work: Work) {
        match self.at_threads < WORK && self.backlog.is_empty() {
            true => self.send_work(work),
            // alloc: past `WORK` at the threads the backlog grows, once, to the most asked.
            false => self.backlog.push_back(work),
        }
    }

    /// `work` onto the threads' queue, which has room: at most [`WORK`] are there.
    fn send_work(&mut self, work: Work) {
        let queue = self.work.as_ref().expect("the driver is running");
        if queue.try_send(work).is_err() {
            panic!("the driver's threads have stopped, or their queue is past its room");
        }
        self.at_threads += 1;
    }

    /// The handles of `file` let go, so the next request opens the file now there: it is
    /// replaced, renamed or removed, or what is under it is.
    fn forget(&mut self, file: &FileName) {
        let files = &mut self.files;
        for names in [&mut self.readers, &mut self.appenders] {
            names.retain(|name, id| {
                let keep = !name.is_under(file);
                if !keep {
                    unname(files, *id);
                }
                keep
            });
        }
    }

    /// `file` opened for the port's requests; the error names it.
    fn open(&mut self, file: &FileName, access: files::Access) -> io::Result<OwnedHandle> {
        if access == files::Access::Append {
            // alloc: a file's first append opens it, made with its directories if missing.
            crate::fs::make_parent(&self.root.join(file.as_str()))?;
        }
        let wide = self.paths.of(file)?;
        files::open(wide, access).map_err(|error| {
            // alloc: an error path.
            io::Error::new(error.kind(), format!("{}: {error}", self.root.join(file.as_str()).display()))
        })
    }

    /// The handle `file` is read or appended through, opened and kept by its name if it is
    /// not yet, with a request on it: [`release`] it as the request ends.
    fn cached(&mut self, file: &FileName, access: files::Access) -> io::Result<(FileId, HANDLE)> {
        let names = match access {
            files::Access::Append => &self.appenders,
            _ => &self.readers,
        };
        if let Some(&id) = names.get(file) {
            let cached = self.files.get_mut(id).expect("a handle kept by its name is open");
            cached.users += 1;
            return Ok((id, cached.handle.as_raw_handle()));
        }
        let handle = self.open(file, access)?;
        self.port.associate(handle.as_raw_handle())?;
        let names = match access {
            files::Access::Append => &mut self.appenders,
            _ => &mut self.readers,
        };
        if names.len() >= CACHED {
            for (_, id) in names.drain() {
                unname(&mut self.files, id);
            }
        }
        let raw = handle.as_raw_handle();
        let id = self.files.insert(Cached { handle, users: 1, named: true });
        // clone: a handle opened, kept by its name.
        names.insert(file.clone(), id);
        Ok((id, raw))
    }

    fn begin(&mut self, effect: Effect) -> io::Result<()> {
        match effect {
            Effect::ReadAll { file, tag } => self.read(&file, 0, None, tag),
            Effect::Read { file, at, len, tag } => self.read(&file, at, Some(len), tag),
            Effect::Append { file, bytes, tag } => {
                let (id, raw) = self.cached(&file, files::Access::Append)?;
                if bytes.is_empty() {
                    release(&mut self.files, id);
                    self.ready.push(Completion { tag, done: Done::Appended });
                    return Ok(());
                }
                let request = self.request(tag, Op::Write { file: id, raw, bytes, at: None, put: 0 });
                self.put(request);
                Ok(())
            }
            Effect::WriteAt { file, at, bytes, tag } => {
                let handle = self.open(&file, files::Access::WriteAt)?;
                if bytes.is_empty() {
                    self.ready.push(Completion { tag, done: Done::Written });
                    return Ok(());
                }
                self.port.associate(handle.as_raw_handle())?;
                let raw = handle.as_raw_handle();
                let id = self.files.insert(Cached { handle, users: 1, named: false });
                let request = self.request(tag, Op::Write { file: id, raw, bytes, at: Some(at), put: 0 });
                self.put(request);
                Ok(())
            }
            // On this thread, as the port's files are opened: its path made in the driver's
            // buffers, where a thread's `sys::path` would allocate one per call.
            Effect::Times { file, modified_ns, tag } => {
                let set = self.paths.of(&file).and_then(|wide| files::set_times_of(wide, modified_ns));
                set.map_err(|error| {
                    // alloc: an error path.
                    io::Error::new(error.kind(), format!("{}: {error}", self.root.join(file.as_str()).display()))
                })?;
                self.ready.push(Completion { tag, done: Done::Timed });
                Ok(())
            }
            effect @ (Effect::Write { .. }
            | Effect::Rename { .. }
            | Effect::Clone { .. }
            | Effect::Link { .. }
            | Effect::Remove { .. }) => {
                match &effect {
                    Effect::Write { file, .. } | Effect::Remove { file, .. } => self.forget(file),
                    Effect::Rename { from, to, .. } => {
                        self.forget(from);
                        self.forget(to);
                    }
                    Effect::Clone { to, .. } | Effect::Link { to, .. } => self.forget(to),
                    _ => {}
                }
                self.to_threads(Work::File(effect));
                Ok(())
            }
            effect @ (Effect::Sync { .. }
            | Effect::Truncate { .. }
            | Effect::Stat { .. }
            | Effect::List { .. }
            | Effect::Mode { .. }
            | Effect::Map { .. }
            | Effect::Stamp { .. }
            | Effect::Entries { .. }) => {
                self.to_threads(Work::File(effect));
                Ok(())
            }
            Effect::Timer { after, tag } => {
                // A timer past what a tick count can say is due in some 584 years: never, here.
                let due = wheel::ticks(self.start.elapsed()).saturating_add(wheel::ticks(after));
                self.timers.try_insert(due, tag).map_err(io::Error::other)?;
                Ok(())
            }
            Effect::Journal { dir, since, tag } => {
                if self.watches.contains_key(&tag) || self.journals.contains_key(&tag) {
                    return Err(watched_already(tag));
                }
                self.journals.insert(tag, Journaling { request: None, stopping: false });
                self.to_threads(Work::Open { tag, dir, since });
                Ok(())
            }
            Effect::Watch { dir, tag } => {
                if self.watches.contains_key(&tag) || self.journals.contains_key(&tag) {
                    return Err(watched_already(tag));
                }
                let buffer = self.buffers.take()?;
                let watching = watch::Watching::new(&mut self.paths, dir, buffer)
                    .and_then(|watching| self.port.associate(watching.raw()).map(|()| watching));
                let watching = match watching {
                    Ok(watching) => watching,
                    Err(error) => {
                        self.buffers.give(buffer);
                        return Err(error);
                    }
                };
                let request = self.request(tag, Op::Watch(watching));
                self.watches.insert(tag, request);
                self.put(request);
                Ok(())
            }
            Effect::Flush { watch, .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                // alloc: an error path.
                format!(
                    "flushing the watch {watch:?}: ReadDirectoryChangesW has no barrier that says \
                     every change made before it has been read, so a watch on Windows vouches for nothing"
                ),
            )),
            Effect::Spawn { command, tag } => self.spawn(command, tag),
            Effect::Listen { at, tag } => self.listen(at, tag),
            Effect::Connect { at, tag } => self.connect(at, tag),
            Effect::Recv { conn, most, tag } => {
                let (id, connection) = self.connection(conn)?;
                if connection.receiving || !connection.recvs.is_empty() {
                    // alloc: a receive behind one in flight; the queue's room is kept for the
                    // connection's life.
                    connection.recvs.push_back((tag, most));
                    return Ok(());
                }
                self.recv(id, tag, most);
                Ok(())
            }
            Effect::Send { conn, bytes, tag } => {
                let (id, connection) = self.connection(conn)?;
                if connection.sending || !connection.sends.is_empty() {
                    // alloc: as for a receive.
                    connection.sends.push_back((tag, bytes));
                    return Ok(());
                }
                self.send(id, tag, bytes);
                Ok(())
            }
            Effect::Hangup { conn, tag } => self.hangup(conn, tag),
            Effect::Kill { .. } | Effect::Unwatch { .. } | Effect::Unlisten { .. } => {
                unreachable!("taken where they are submitted")
            }
        }
    }

    fn read(&mut self, file: &FileName, at: u64, len: Option<usize>, tag: Tag) -> io::Result<()> {
        let (id, raw) = match self.cached(file, files::Access::Read) {
            Ok(it) => it,
            // A file that is not there reads as empty, read whole.
            Err(error) if error.kind() == io::ErrorKind::NotFound && len.is_none() => {
                self.ready.push(Completion { tag, done: Done::Read(Vec::new()) }); // alloc: none, an empty `Vec`.
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let size = match len {
            Some(len) => Ok(len),
            // A byte past its length, so the read that fills it is the one that finds the end.
            None => files::size(raw).map(|size| size as usize + 1),
        };
        let size = match size {
            Ok(0) => {
                release(&mut self.files, id);
                self.ready.push(Completion { tag, done: Done::Read(Vec::new()) }); // alloc: none, an empty `Vec`.
                return Ok(());
            }
            Ok(size) => size,
            Err(error) => {
                release(&mut self.files, id);
                return Err(error);
            }
        };
        // alloc: the bytes read, `Done::Read`'s payload, which the kernel writes in place.
        let bytes = Vec::with_capacity(size);
        let request = self.request(tag, Op::Read { file: id, raw, bytes, at, want: size, to_end: len.is_none() });
        self.put(request);
        Ok(())
    }

    fn spawn(&mut self, command: Box<Command>, tag: Tag) -> io::Result<()> {
        if self.spawning.contains_key(&tag) {
            // alloc: an error path.
            return Err(io::Error::other(format!("a process runs as {tag:?} already")));
        }
        let stdout_buffer = self.buffers.take()?;
        let stderr_buffer = match self.buffers.take() {
            Ok(buffer) => buffer,
            Err(error) => {
                self.buffers.give(stdout_buffer);
                return Err(error);
            }
        };
        let started = self.start_spawn(command, tag, [stdout_buffer, stderr_buffer]);
        if started.is_err() {
            self.buffers.give(stdout_buffer);
            self.buffers.give(stderr_buffer);
        }
        started
    }

    /// The process of `command` started and followed, its standard output and error read into
    /// `buffers` (theirs from here, unless it fails).
    fn start_spawn(&mut self, command: Box<Command>, tag: Tag, buffers: [Buffer; 2]) -> io::Result<()> {
        let started = process::start(&self.root, command, self.confine.as_ref())?;
        let process::Started { process, thread, job, stdin, stdout, stderr, mut finish } = started;
        let port = &self.port;
        let associated = [Some(&stdout), Some(&stderr), stdin.as_ref()]
            .into_iter()
            .flatten()
            .try_for_each(|pipe| port.associate(pipe.as_raw_handle()));
        if let Err(error) = associated {
            drop((stdin, stdout, stderr));
            abandon(&process, &job, &finish);
            return Err(error);
        }
        // The input the request writing it owns, as the kernel reads it until its completion.
        let input = std::mem::take(&mut finish.command.stdin);
        let spawn = self.spawns.insert_with(|id| Spawned {
            tag,
            process,
            job,
            waiter: process::Waiter::new(port, id.to_bits()),
            waiting: None,
            // alloc: none until output comes, `Exit`'s payload.
            out: Vec::new(),
            err: Vec::new(), // alloc: as above.
            outputs: 2,
            exited: false,
            input: None,
            failed: None,
            finish,
        });
        let spawned = self.spawns.get_mut(spawn).expect("just made");
        let followed = process::follow(&spawned.job, &self.port, spawn.to_bits() as usize).and_then(|()| {
            // SAFETY: the waiter is in the spawn's entry of the slab, which never moves while it
            // is there; `waiting` is dropped before the spawn leaves it (`end_wait`).
            unsafe { process::Waiting::new(&spawned.process, &spawned.waiter) }
        });
        let resumed = followed.and_then(|waiting| {
            spawned.waiting = Some(waiting);
            process::resume(thread, &spawned.job)
        });
        if let Err(error) = resumed {
            let spawned = self.end_wait(spawn).expect("just made");
            abandon(&spawned.process, &spawned.job, &spawned.finish);
            return Err(error);
        }
        self.spawning.insert(tag, spawn);
        let [stdout_buffer, stderr_buffer] = buffers;
        for (err, pipe, buffer) in [(false, stdout, stdout_buffer), (true, stderr, stderr_buffer)] {
            let request = self.request(tag, Op::Output { spawn, err, pipe, buffer });
            self.put(request);
        }
        if let Some(pipe) = stdin {
            let request = self.request(tag, Op::Input { spawn, pipe, bytes: input, put: 0 });
            if let Some(spawned) = self.spawns.get_mut(spawn) {
                spawned.input = Some(request);
            }
            self.put(request);
        }
        Ok(())
    }

    /// The spawn `id` taken out of the slab, its wait unregistered first, so the thread pool
    /// reads its waiter no more.
    fn end_wait(&mut self, id: SpawnId) -> Option<Spawned> {
        drop(self.spawns.get_mut(id)?.waiting.take());
        self.spawns.remove(id)
    }

    /// Ends the spawn `id` once it has exited and its output has ended.
    fn finish_spawn(&mut self, id: SpawnId) {
        match self.spawns.get(id) {
            Some(it) if it.exited && it.outputs == 0 => {}
            _ => return,
        }
        let spawned = self.end_wait(id).expect("looked up above");
        // What it did not read of its input is not written.
        if let Some(request) = spawned.input.and_then(|request| self.requests.get(request)) {
            request.cancel();
        }
        let Spawned { tag, process, job, out, err, failed, finish, .. } = spawned;
        self.spawning.remove(&tag);
        let ended = finish.exited(&process, &job, out, err);
        // Closing the job kills whatever of it is left (`KILL_ON_JOB_CLOSE`).
        drop(job);
        let done = match (ended, failed) {
            // alloc: `Done::Exited`'s payload.
            (Ok(exit), None) => Done::Exited(Box::new(exit)),
            (Ok(_), Some(error)) | (Err(error), _) => {
                // alloc: an error path.
                Done::Failed(IoError(format!("{}: {error}", finish.program().display())))
            }
        };
        self.ready.push(Completion { tag, done });
    }

    fn listen(&mut self, at: Address, tag: Tag) -> io::Result<()> {
        if self.listens.contains_key(&tag) {
            // alloc: an error path.
            return Err(io::Error::other(format!("something listens as {tag:?} already")));
        }
        // alloc: an error path.
        let context = |error: io::Error| io::Error::new(error.kind(), format!("listening at {at:?}: {error}"));
        let listener = net::listen(&self.root, &at).map_err(context)?;
        self.port.associate(net::raw(&listener.socket) as HANDLE).map_err(context)?;
        let request = self.request(tag, Op::Accept(net::Accepting::new(listener)));
        self.listens.insert(tag, request);
        self.put(request);
        Ok(())
    }

    fn connect(&mut self, at: Address, tag: Tag) -> io::Result<()> {
        if let Address::Unix(_) = at {
            self.to_threads(Work::Connect { tag, at });
            return Ok(());
        }
        // alloc: an error path.
        let context = |at: &Address, error: io::Error| io::Error::new(error.kind(), format!("connecting to {at:?}: {error}"));
        let connecting = net::Connecting::new(&self.root, &at).map_err(|error| context(&at, error))?;
        let id = self.request(tag, Op::Connect { connecting, at });
        let port = &self.port;
        let Request { head, op, .. } = self.requests.get_mut(id).expect("just made");
        let Op::Connect { connecting, at } = op else { unreachable!("just made") };
        let associate = |socket: &OwnedSocket| port.associate(net::raw(socket) as HANDLE);
        if let Err(error) = connecting.start(associate, head.reset(0)) {
            let error = context(at, error);
            // Refused before it was asked for: no completion comes.
            self.requests.remove(id);
            return Err(error);
        }
        Ok(())
    }

    /// A connect's end: `made`, its socket on the port, or why not.
    fn connected(&mut self, tag: Tag, at: &Address, made: io::Result<OwnedSocket>) {
        match made {
            Ok(socket) => {
                let conn = adopt(&mut self.conns, socket);
                self.ready.push(Completion { tag, done: Done::Connected { conn } });
            }
            Err(error) => self.fail(tag, format_args!("connecting to {at:?}: {error}")),
        }
    }

    fn connection(&mut self, conn: Conn) -> io::Result<(ConnId, &mut Connection)> {
        let id = ConnId::from_bits(conn.0);
        match self.conns.get_mut(id) {
            Some(connection) if !connection.hung_up => Ok((id, connection)),
            _ => Err(no_connection(conn)),
        }
    }

    /// A receive of up to `most` bytes on `id`, which has none in flight.
    fn recv(&mut self, id: ConnId, tag: Tag, most: usize) {
        let connection = self.conns.get_mut(id).expect("the caller's connection");
        connection.receiving = true;
        let socket = net::raw(&connection.socket);
        // alloc: the bytes received, `Done::Read`'s payload, which the kernel writes in place.
        let bytes = Vec::with_capacity(most.min(BUFFER));
        let op = Op::Recv { conn: id, socket, bytes, flags: UnsafeCell::new(0) };
        let request = self.request(tag, op);
        self.put(request);
    }

    /// A send of `bytes` on `id`, which has none in flight; a send of none is sent at once.
    fn send(&mut self, id: ConnId, tag: Tag, bytes: Vec<u8>) {
        if bytes.is_empty() {
            self.ready.push(Completion { tag, done: Done::Sent });
            return;
        }
        let connection = self.conns.get_mut(id).expect("the caller's connection");
        connection.sending = true;
        let socket = net::raw(&connection.socket);
        let request = self.request(tag, Op::Send { conn: id, socket, bytes, put: 0 });
        self.put(request);
    }

    /// The next receive asked of `id` asked for, now none is in flight; or, hung up, closed.
    fn serve_recvs(&mut self, id: ConnId) {
        loop {
            let Some(connection) = self.conns.get_mut(id) else { return };
            if connection.hung_up {
                return self.close_if_idle(id);
            }
            if connection.receiving {
                return;
            }
            let Some((tag, most)) = connection.recvs.pop_front() else { return };
            self.recv(id, tag, most);
        }
    }

    /// As [`Iocp::serve_recvs`], of sends.
    fn serve_sends(&mut self, id: ConnId) {
        loop {
            let Some(connection) = self.conns.get_mut(id) else { return };
            if connection.hung_up {
                return self.close_if_idle(id);
            }
            if connection.sending {
                return;
            }
            let Some((tag, bytes)) = connection.sends.pop_front() else { return };
            self.send(id, tag, bytes);
        }
    }

    /// The connection `id` closed if it is hung up and nothing is in flight on it.
    fn close_if_idle(&mut self, id: ConnId) {
        if self.conns.get(id).is_some_and(|it| it.hung_up && !it.receiving && !it.sending) {
            self.conns.remove(id);
        }
    }

    fn hangup(&mut self, conn: Conn, tag: Tag) -> io::Result<()> {
        let id = ConnId::from_bits(conn.0);
        let Some(connection) = self.conns.get_mut(id).filter(|it| !it.hung_up) else {
            return Err(no_connection(conn));
        };
        connection.hung_up = true;
        let shut = net::hang_up(&connection.socket);
        // What is in flight on it ends now: a receive as empty, a send as failed (`ended`).
        // SAFETY: an open socket; no `OVERLAPPED` names every request on it.
        unsafe { CancelIoEx(net::raw(&connection.socket) as HANDLE, std::ptr::null()) };
        for (recv, _) in connection.recvs.drain(..) {
            self.ready.push(Completion { tag: recv, done: Done::Read(Vec::new()) }); // alloc: none, an empty `Vec`.
        }
        for (send, _) in connection.sends.drain(..) {
            self.ready.push(failed(send, format_args!("{conn:?} hung up before all of it was sent")));
        }
        self.close_if_idle(id);
        shut?;
        self.ready.push(Completion { tag, done: Done::HungUp });
        Ok(())
    }

    /// What a request's step came to: its next step asked for, in place, or its end.
    fn ended(&mut self, id: Id, result: io::Result<u32>) {
        let Some(request) = self.requests.get_mut(id) else { return };
        if matches!(request.op, Op::Connect { .. } | Op::Journal(_)) {
            let request = self.requests.remove(id).expect("looked up above");
            let tag = request.tag;
            match request.op {
                Op::Connect { connecting, at } => {
                    let made = result.and_then(|_| connecting.take());
                    self.connected(tag, &at, made);
                }
                Op::Journal(journal) => self.journal_read(tag, journal, result),
                _ => unreachable!("matched above"),
            }
            return;
        }
        let tag = request.tag;
        let code = result.as_ref().err().and_then(io::Error::raw_os_error).map(|it| it as u32);
        let after = 'step: {
            match &mut request.op {
                Op::Read { bytes, want, to_end, .. } => {
                    let got = match result {
                        Ok(got) => got as usize,
                        Err(_) if code == Some(ERROR_HANDLE_EOF) => 0,
                        Err(error) => {
                            self.ready.push(failed(tag, error));
                            break 'step After::Over(Then::Nothing);
                        }
                    };
                    let filled = bytes.len() + got.min(*want - bytes.len());
                    // SAFETY: the read's completion was taken: the kernel wrote `got` bytes from
                    // the start of the spare room it was given (at most `want` less what was
                    // there), and writes them no more.
                    unsafe { bytes.set_len(filled) };
                    if got > 0 && *to_end && filled == *want {
                        *want *= 2;
                        // alloc: the file grew after its length was asked.
                        bytes.reserve(*want - filled);
                    }
                    if got > 0 && filled < *want {
                        break 'step After::Again;
                    }
                    let bytes = std::mem::take(bytes);
                    self.ready.push(Completion { tag, done: Done::Read(bytes) });
                    After::Over(Then::Nothing)
                }
                Op::Write { bytes, at, put, .. } => {
                    let done = match result {
                        Ok(0) if *put < bytes.len() => Done::Failed(IoError("the file took no more bytes".into())),
                        Ok(got) => {
                            *put += got as usize;
                            if *put < bytes.len() {
                                break 'step After::Again;
                            }
                            match at {
                                Some(_) => Done::Written,
                                None => Done::Appended,
                            }
                        }
                        // alloc: an error path.
                        Err(error) => Done::Failed(IoError(error.to_string())),
                    };
                    self.ready.push(Completion { tag, done });
                    After::Over(Then::Nothing)
                }
                Op::Watch(watching) => {
                    let stopping = watching.stopping;
                    match result {
                        Ok(got) => {
                            // A buffer of nothing while stopping is the cancel, not a loss.
                            if !(stopping && got == 0) {
                                let changed = watching.take(got as usize, &mut self.paths, &mut self.scratch);
                                self.ready.push(Completion { tag, done: changed });
                            }
                            if !stopping {
                                break 'step After::Again;
                            }
                        }
                        Err(_) if code == Some(ERROR_NOTIFY_ENUM_DIR) && !stopping => {
                            self.ready.push(Completion { tag, done: Done::Changed { files: Vec::new(), lost: true } }); // alloc: none, an empty `Vec`.
                            break 'step After::Again;
                        }
                        Err(_) if stopping => {}
                        Err(error) => {
                            self.watches.remove(&tag);
                            self.ready.push(failed(tag, format_args!("watching: {error}")));
                            break 'step After::Over(Then::Nothing);
                        }
                    }
                    self.watches.remove(&tag);
                    self.ready.push(Completion { tag, done: Done::Unwatched });
                    After::Over(Then::Nothing)
                }
                Op::Output { spawn, err, buffer, .. } => {
                    let spawn = *spawn;
                    let Some(spawned) = self.spawns.get_mut(spawn) else { break 'step After::Over(Then::Nothing) };
                    match result {
                        Ok(got) => {
                            // SAFETY: the read's completion was taken, so the kernel writes the
                            // buffer no more; it wrote `got` bytes of it.
                            let read = unsafe { std::slice::from_raw_parts(buffer.ptr(), (got as usize).min(BUFFER)) };
                            let into = if *err { &mut spawned.err } else { &mut spawned.out };
                            // alloc: the process's output, `Exit`'s payload, grown as it comes.
                            into.extend_from_slice(read);
                            break 'step After::Again;
                        }
                        // The last writer closed it: the end of the output.
                        Err(_) if code == Some(ERROR_BROKEN_PIPE) => {}
                        Err(error) => {
                            process::terminate(&spawned.job, process::KILLED);
                            spawned.failed.get_or_insert(error);
                        }
                    }
                    spawned.outputs -= 1;
                    After::Over(Then::Spawn(spawn))
                }
                Op::Input { spawn, bytes, put, .. } => {
                    let Some(spawned) = self.spawns.get_mut(*spawn) else { break 'step After::Over(Then::Nothing) };
                    match result {
                        Ok(got) => {
                            *put += got as usize;
                            if got > 0 && *put < bytes.len() {
                                break 'step After::Again;
                            }
                        }
                        // It stopped reading: what is left of its input is not wanted.
                        Err(_) if matches!(code, Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_OPERATION_ABORTED)) => {}
                        Err(error) => {
                            process::terminate(&spawned.job, process::KILLED);
                            spawned.failed.get_or_insert(error);
                        }
                    }
                    // Closed with the request: the end of its input.
                    spawned.input = None;
                    After::Over(Then::Nothing)
                }
                Op::Accept(accepting) => {
                    if accepting.stopping {
                        // A connection accepted as it was asked to stop closes with it.
                        self.listens.remove(&tag);
                        self.ready.push(Completion { tag, done: Done::Unlistened });
                        break 'step After::Over(Then::Nothing);
                    }
                    match result {
                        Ok(_) => {
                            let port = &self.port;
                            let taken = accepting.take().and_then(|socket| {
                                port.associate(net::raw(&socket) as HANDLE)?;
                                Ok(socket)
                            });
                            match taken {
                                Ok(socket) => {
                                    let conn = adopt(&mut self.conns, socket);
                                    self.ready.push(Completion { tag, done: Done::Accepted { conn } });
                                }
                                Err(error) => {
                                    self.listens.remove(&tag);
                                    self.ready.push(failed(tag, format_args!("accepting a connection: {error}")));
                                    break 'step After::Over(Then::Nothing);
                                }
                            }
                        }
                        // The peer went before it was accepted.
                        Err(_)
                            if matches!(code, Some(ERROR_NETNAME_DELETED | ERROR_CONNECTION_ABORTED))
                                || code == Some(WSAECONNRESET as u32) => {}
                        Err(error) => {
                            self.listens.remove(&tag);
                            self.ready.push(failed(tag, format_args!("accepting a connection: {error}")));
                            break 'step After::Over(Then::Nothing);
                        }
                    }
                    After::Again
                }
                Op::Connect { .. } | Op::Journal(_) => unreachable!("ended above"),
                Op::Recv { conn, bytes, .. } => {
                    let done = match result {
                        Ok(got) => {
                            // SAFETY: the receive's completion was taken: the kernel wrote `got`
                            // bytes from the start of the room it was given, which is the
                            // `Vec`'s capacity, and writes them no more.
                            unsafe { bytes.set_len((got as usize).min(bytes.capacity())) };
                            Done::Read(std::mem::take(bytes))
                        }
                        // Hung up, here or by the peer: no more bytes.
                        Err(_)
                            if matches!(
                                code,
                                Some(ERROR_OPERATION_ABORTED | ERROR_NETNAME_DELETED | ERROR_CONNECTION_ABORTED)
                            ) || code == Some(WSAECONNRESET as u32) =>
                        {
                            Done::Read(Vec::new()) // alloc: none, an empty `Vec`.
                        }
                        // alloc: an error path.
                        Err(error) => Done::Failed(IoError(error.to_string())),
                    };
                    self.ready.push(Completion { tag, done });
                    if let Some(connection) = self.conns.get_mut(*conn) {
                        connection.receiving = false;
                    }
                    After::Over(Then::Recv(*conn))
                }
                Op::Send { conn, bytes, put, .. } => {
                    let conn = *conn;
                    let shown = Conn(conn.to_bits());
                    let open = self.conns.get(conn).is_some_and(|it| !it.hung_up);
                    let done = match result {
                        Ok(got) => {
                            *put += got as usize;
                            if got > 0 && *put < bytes.len() && open {
                                break 'step After::Again;
                            }
                            match *put == bytes.len() {
                                true => Done::Sent,
                                // alloc: an error path.
                                false => Done::Failed(IoError(format!("{shown:?} took no more bytes"))),
                            }
                        }
                        Err(_) if code == Some(ERROR_OPERATION_ABORTED) => {
                            // alloc: an error path.
                            Done::Failed(IoError(format!("{shown:?} hung up before all of it was sent")))
                        }
                        // alloc: an error path.
                        Err(error) => Done::Failed(IoError(error.to_string())),
                    };
                    self.ready.push(Completion { tag, done });
                    if let Some(connection) = self.conns.get_mut(conn) {
                        connection.sending = false;
                    }
                    After::Over(Then::Send(conn))
                }
            }
        };
        let then = match after {
            After::Again => return self.put(id),
            After::Over(then) => then,
        };
        if let Some(request) = self.requests.remove(id) {
            self.let_go(request);
        }
        match then {
            Then::Nothing => {}
            Then::Spawn(spawn) => self.finish_spawn(spawn),
            Then::Recv(conn) => self.serve_recvs(conn),
            Then::Send(conn) => self.serve_sends(conn),
        }
    }

    /// What an ended request held of the driver's given back.
    fn let_go(&mut self, request: Request) {
        match &request.op {
            Op::Read { file, .. } | Op::Write { file, .. } => release(&mut self.files, *file),
            Op::Watch(watching) => self.buffers.give(watching.buffer),
            Op::Output { buffer, .. } => self.buffers.give(*buffer),
            _ => {}
        }
    }

    /// The request a [`KEY_IO`] packet is the completion of; `None` where it names one that
    /// has ended (its slot free, or another's since).
    fn owner(&self, entry: &OVERLAPPED_ENTRY) -> Option<Id> {
        // SAFETY: a packet of this key hands back a head's `OVERLAPPED`: a request's, in the
        // slab, whose memory stays mapped while the driver lives (`requests`), or a test's own.
        let id = Id::from_bits(unsafe { Head::id_of(entry.lpOverlapped) });
        let request = self.requests.get(id)?;
        (request.head.overlapped().cast_const() == entry.lpOverlapped.cast_const()).then_some(id)
    }

    /// Takes what the port has, waiting up to `wait` for the first, and acts on it.
    fn wait(&mut self, wait: Duration) {
        let got = self.port.take(&mut self.entries, wait).expect("waiting on the completion port");
        for at in 0..got {
            let entry = self.entries[at];
            match entry.lpCompletionKey {
                KEY_IO => {
                    let Some(id) = self.owner(&entry) else {
                        self.stale += 1;
                        continue;
                    };
                    let request = self.requests.get(id).expect("the packet's owner");
                    let result = request.head.result().map_err(|code| io::Error::from_raw_os_error(code as i32));
                    self.ended(id, result);
                }
                KEY_WAKE => {}
                KEY_EXIT => {
                    let id = SpawnId::from_bits(entry.lpOverlapped as usize as u64);
                    let Some(spawned) = self.spawns.get_mut(id) else { continue };
                    spawned.exited = true;
                    self.finish_spawn(id);
                }
                key if key >= KEY_JOB => {
                    let Some(spawned) = self.spawns.get(SpawnId::from_bits(key as u64)) else { continue };
                    // Past its memory: the commit the job is refused is the backstop, and the
                    // kill makes it the end.
                    if entry.dwNumberOfBytesTransferred == JOB_OBJECT_MSG_JOB_MEMORY_LIMIT {
                        process::terminate(&spawned.job, process::OUT_OF_MEMORY);
                    }
                }
                _ => {}
            }
        }
    }

    /// Every timer due fired, in the order they were due, and what the threads sent taken.
    fn gather(&mut self, out: &mut Vec<Completion>) {
        let ready = &mut self.ready;
        self.timers.advance(wheel::ticks(self.start.elapsed()), |_, tag| {
            ready.push(Completion { tag, done: Done::Fired });
        });
        // Cleared before the drain: a thread that finds it set after this sent before it, so
        // the drain sees what it sent (both are read-modify-writes, so they synchronize).
        self.woken.swap(false, Ordering::AcqRel);
        while let Ok(back) = self.back.try_recv() {
            self.at_threads -= 1;
            match back {
                Back::Done(completion) => self.ready.push(completion),
                Back::Connected { tag, at, made } => {
                    let made = made.and_then(|socket| {
                        self.port.associate(net::raw(&socket) as HANDLE)?;
                        Ok(socket)
                    });
                    self.connected(tag, &at, made);
                }
                Back::Journal { tag, journal, taken } => self.journaled(tag, journal, taken),
            }
        }
        while self.at_threads < WORK {
            let Some(work) = self.backlog.pop_front() else { break };
            self.send_work(work);
        }
        out.append(&mut self.ready);
    }

    /// A journal the threads handed back: its batch told, then its next read asked for, or its
    /// end, or, asked to end meanwhile, its unwatch.
    fn journaled(&mut self, tag: Tag, journal: Option<Box<usn::Journal>>, taken: usn::Taken) {
        let Some(journaling) = self.journals.get(&tag) else { return };
        let stopping = journaling.stopping;
        if let Some(batch) = taken.batch {
            self.ready.push(Completion { tag, done: Done::Journaled(batch) });
        }
        if let Some(why) = taken.end {
            self.journals.remove(&tag);
            return self.fail(tag, why);
        }
        if stopping {
            self.journals.remove(&tag);
            self.ready.push(Completion { tag, done: Done::Unwatched });
            return;
        }
        let journal = journal.expect("a journal handed back without an end is handed back with it");
        let request = self.request(tag, Op::Journal(journal));
        if let Some(journaling) = self.journals.get_mut(&tag) {
            journaling.request = Some(request);
        }
        self.put(request);
    }

    /// A read of the journal `tag` ended: its records to be named on the threads, or, asked to
    /// end, the journal let go. What it read as it was cancelled is after the last batch's
    /// cursor, so a watch from that cursor reads it again.
    fn journal_read(&mut self, tag: Tag, journal: Box<usn::Journal>, result: io::Result<u32>) {
        let Some(journaling) = self.journals.get_mut(&tag) else { return };
        journaling.request = None;
        if journaling.stopping {
            self.journals.remove(&tag);
            drop(journal);
            self.ready.push(Completion { tag, done: Done::Unwatched });
            return;
        }
        self.to_threads(Work::Took { tag, journal, result });
    }
}

impl Drop for Iocp {
    fn drop(&mut self) {
        for (_, spawned) in self.spawns.iter() {
            process::terminate(&spawned.job, process::KILLED);
        }
        for (_, request) in self.requests.iter() {
            request.cancel();
        }
        // Each request is let go only once the port gives it back: until then the kernel may
        // still write to what it lent.
        let deadline = Instant::now() + GIVE_BACK;
        while !self.requests.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let Ok(got) = self.port.take(&mut self.entries, left) else { break };
            for at in 0..got {
                let entry = self.entries[at];
                if entry.lpCompletionKey == KEY_IO
                    && let Some(id) = self.owner(&entry)
                {
                    drop(self.requests.remove(id));
                }
            }
        }
        // What never came back stays lent: the slab its heads are in and the buffers are left
        // mapped (leaked), so nothing the kernel may write is freed.
        if self.requests.is_empty() {
            // SAFETY: dropped once, here, with no request in flight to write either.
            unsafe {
                ManuallyDrop::drop(&mut self.requests);
                ManuallyDrop::drop(&mut self.buffers);
            }
        }
        for (_, spawned) in self.spawns.iter_mut() {
            drop(spawned.waiting.take());
            abandon(&spawned.process, &spawned.job, &spawned.finish);
        }
        self.work = None;
        for worker in self.workers.drain(..) {
            _ = worker.join();
        }
    }
}

impl Driver for Iocp {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            let tag = effect.tag();
            match effect {
                Effect::Kill { tag } => {
                    if let Some(spawned) = self.spawning.get(&tag).and_then(|id| self.spawns.get(*id)) {
                        process::terminate(&spawned.job, process::KILLED);
                    }
                }
                // The end of the watch or listen, in flight since it was asked for, comes with
                // its request's completion: after every change it heard. A journal on the
                // threads ends as they hand it back.
                Effect::Unwatch { tag } if self.journals.contains_key(&tag) => {
                    let journaling = self.journals.get_mut(&tag).expect("looked up above");
                    journaling.stopping = true;
                    if let Some(request) = journaling.request.and_then(|id| self.requests.get(id)) {
                        request.cancel();
                    }
                }
                Effect::Unwatch { tag } => match self.watches.get(&tag).and_then(|id| self.requests.get_mut(*id)) {
                    Some(request) => {
                        if let Op::Watch(watching) = &mut request.op {
                            watching.stopping = true;
                        }
                        request.cancel();
                    }
                    None => {
                        self.in_flight += 1;
                        self.fail(tag, format_args!("nothing is watched as {tag:?}"));
                    }
                },
                Effect::Unlisten { tag } => match self.listens.get(&tag).and_then(|id| self.requests.get_mut(*id)) {
                    Some(request) => {
                        if let Op::Accept(accepting) = &mut request.op {
                            accepting.stopping = true;
                        }
                        request.cancel();
                    }
                    None => {
                        self.in_flight += 1;
                        self.fail(tag, format_args!("nothing listens as {tag:?}"));
                    }
                },
                effect => {
                    self.in_flight += 1;
                    if let Err(error) = self.begin(effect) {
                        self.fail(tag, error);
                    }
                }
            }
        }
    }

    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize {
        let before = out.len();
        let deadline = Instant::now().checked_add(wait);
        loop {
            self.gather(out);
            if out.len() > before || self.in_flight == 0 {
                break;
            }
            let now = Instant::now();
            let mut left = deadline.map_or(Duration::MAX, |it| it.saturating_duration_since(now));
            if let Some(due) = self.timers.next_deadline() {
                let at = wheel::ticks(now.saturating_duration_since(self.start));
                left = left.min(Duration::from_nanos(due.saturating_sub(at)));
            }
            self.wait(left);
            self.gather(out);
            if out.len() > before || deadline.is_some_and(|it| Instant::now() >= it) {
                break;
            }
        }
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

fn failed(tag: Tag, why: impl fmt::Display) -> Completion {
    // alloc: an error path.
    Completion { tag, done: Done::Failed(IoError(why.to_string())) }
}

fn watched_already(tag: Tag) -> io::Error {
    // alloc: an error path.
    io::Error::other(format!("something is watched as {tag:?} already"))
}

/// `socket` kept as a connection: what names it is its handle's bits.
fn adopt(conns: &mut Slab<Connection>, socket: OwnedSocket) -> Conn {
    let id = conns.insert(Connection {
        socket,
        recvs: VecDeque::new(),
        sends: VecDeque::new(),
        receiving: false,
        sending: false,
        hung_up: false,
    });
    Conn(id.to_bits())
}

fn work_on(root: &Path, queue: &Receiver<Work>, port: &Port, back: &Sender<Back>, woken: &AtomicBool) {
    // alloc: setup, the paths this thread's file effects are joined in, grown to the longest.
    let (mut at, mut to) = (PathBuf::with_capacity(512), PathBuf::with_capacity(512));
    while let Ok(work) = queue.recv() {
        let sent = match work {
            Work::File(effect) => {
                let tag = effect.tag();
                let done = file(root, effect, &mut at, &mut to)
                    // alloc: an error path.
                    .unwrap_or_else(|error| Done::Failed(IoError(error.to_string())));
                back.send(Back::Done(Completion { tag, done }))
            }
            Work::Connect { tag, at } => {
                let made = net::connect_blocking(root, &at);
                back.send(Back::Connected { tag, at, made })
            }
            Work::Open { tag, dir, since } => {
                let opened = usn::Journal::open(root, &dir, since).and_then(|(journal, first)| {
                    port.associate(journal.raw())?;
                    Ok((journal, first))
                });
                let (journal, taken) = match opened {
                    Ok((journal, batch)) => (Some(journal), usn::Taken { batch, end: None }),
                    // alloc: an error path.
                    Err(error) => (None, usn::Taken { batch: None, end: Some(error.to_string()) }),
                };
                back.send(Back::Journal { tag, journal, taken })
            }
            Work::Took { tag, mut journal, result } => {
                let taken = journal.take(result);
                let journal = taken.end.is_none().then_some(journal);
                back.send(Back::Journal { tag, journal, taken })
            }
        };
        if sent.is_err() {
            return;
        }
        if !woken.swap(true, Ordering::AcqRel) && port.post(KEY_WAKE, 0, 0).is_err() {
            return;
        }
    }
}

/// `file` under `root`, joined in `into`.
fn joined<'a>(into: &'a mut PathBuf, root: &Path, file: &FileName) -> &'a Path {
    into.as_mut_os_string().clear();
    into.push(root);
    into.push(file.as_str());
    into
}

/// A file effect that blocks, as Windows does it.
fn file(root: &Path, effect: Effect, at: &mut PathBuf, to: &mut PathBuf) -> io::Result<Done> {
    match effect {
        Effect::Write { file, bytes, .. } => {
            files::write_atomically(joined(at, root, &file), &bytes).map(|()| Done::Written)
        }
        Effect::Rename { from, to: into, .. } => {
            files::rename(joined(at, root, &from), joined(to, root, &into)).map(|()| Done::Renamed)
        }
        Effect::Mode { file, mode, .. } => files::set_mode(joined(at, root, &file), mode).map(|()| Done::Moded),
        Effect::Clone { from, to: into, .. } => {
            files::clone(joined(at, root, &from), joined(to, root, &into)).map(|()| Done::Cloned)
        }
        Effect::Map { file, .. } => files::map(joined(at, root, &file)).map(Done::Mapped),
        effect => crate::threads::run(root, effect),
    }
}

/// A process the driver no longer follows: killed, waited for, and what was made for it
/// removed.
fn abandon(process: &OwnedHandle, job: &OwnedHandle, finish: &process::Finish) {
    process::terminate(job, process::KILLED);
    // SAFETY: an open process handle; a kill of its job ends it promptly.
    unsafe { WaitForSingleObject(process.as_raw_handle(), GIVE_BACK.as_millis() as u32) };
    _ = finish.exited(process, job, Vec::new(), Vec::new()); // alloc: none, an empty `Vec`.
}

fn no_connection(conn: Conn) -> io::Error {
    // alloc: an error path.
    io::Error::new(io::ErrorKind::NotConnected, format!("no connection {conn:?}"))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString};

    use super::*;
    use crate::{Confined, Exit, Journaled, Lost, Names, Sandbox, Stamp, Traced, run_until_idle, threads::Threads};

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    fn dir(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-iocp-{what}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn system() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set")).join("System32")
    }

    /// `program` of System32 with `args`; its environment is only `SystemRoot`, which much of
    /// Windows needs to start.
    fn command(program: &str, args: &[&str], stdin: &[u8]) -> Box<Command> {
        Box::new(Command {
            program: system().join(program),
            args: args.iter().map(|it| OsString::from(*it)).collect(),
            env: vec![("SystemRoot".into(), std::env::var_os("SystemRoot").unwrap())],
            dir: std::env::temp_dir(),
            stdin: stdin.to_vec(),
            sandbox: Sandbox::Off,
            memory: None,
        })
    }

    fn run(driver: &mut impl Driver, effects: Vec<Effect>) -> BTreeMap<u64, Done> {
        let mut effects = effects;
        let mut seen = BTreeMap::new();
        run_until_idle(driver, &mut effects, |done, _| _ = seen.insert(done.tag.0, done.done));
        assert_eq!(driver.in_flight(), 0);
        seen
    }

    fn exit(done: &Done) -> &Exit {
        match done {
            Done::Exited(exit) => exit,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn processes_end_with_their_output_code_and_kill() {
        let root = dir("spawn");
        let mut driver = Iocp::new(&root, 1).unwrap();
        // More than a pipe holds each way, read back while it is written.
        let big: Vec<u8> = (0..40_000).flat_map(|n| format!("line {n:05}\r\n").into_bytes()).collect();
        let limit = 256 << 20;
        let hog = Box::new(Command {
            memory: Some(limit),
            ..*command(
                r"WindowsPowerShell\v1.0\powershell.exe",
                &["-NoProfile", "-NonInteractive", "-Command", "$a = New-Object byte[] 1GB; $a.Length"],
                b"",
            )
        });
        let mut on = command("cmd.exe", &["/d", "/c", "echo hi"], b"");
        on.sandbox = Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: false };
        let seen = run(&mut driver, vec![
            Effect::Spawn { command: command("findstr.exe", &["/r", "^"], b"hello\r\n"), tag: Tag(1) },
            Effect::Spawn { command: command("cmd.exe", &["/d", "/c", "echo err>&2& exit /b 7"], b""), tag: Tag(2) },
            Effect::Spawn { command: command("PING.EXE", &["-n", "60", "127.0.0.1"], b""), tag: Tag(3) },
            Effect::Kill { tag: Tag(3) },
            Effect::Spawn { command: on, tag: Tag(4) },
            Effect::Spawn { command: hog, tag: Tag(5) },
            Effect::Spawn { command: command("findstr.exe", &["/r", "^"], &big), tag: Tag(6) },
            Effect::Kill { tag: Tag(99) },
        ]);
        let one = exit(&seen[&1]);
        assert_eq!((one.code, one.signal), (Some(0), None));
        assert_eq!((one.stdout.as_slice(), one.stderr.as_slice()), (&b"hello\r\n"[..], &b""[..]));
        assert!(one.peak_memory > 0);
        let two = exit(&seen[&2]);
        assert_eq!((two.code, two.stdout.as_slice(), two.stderr.as_slice()), (Some(7), &b""[..], &b"err\r\n"[..]));
        let three = exit(&seen[&3]);
        assert_eq!((three.code, three.signal), (Some(process::KILLED as i32), None));
        assert!(matches!(&seen[&4], Done::Failed(why) if why.0.contains("Confine")), "{:?}", seen[&4]);
        // Refused its commit past the limit, or killed going past it: never all of it.
        let five = exit(&seen[&5]);
        assert_ne!(five.code, Some(0), "{five:?}");
        assert!(five.peak_memory > 0 && five.peak_memory <= limit, "{}", five.peak_memory);
        let six = exit(&seen[&6]);
        assert_eq!(six.code, Some(0));
        assert!(six.stdout == big, "{} bytes back of {}", six.stdout.len(), big.len());
        assert!(!seen.contains_key(&99));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Starts the program after a launcher that leaves a record in the scratch directory.
    struct Recording;

    impl Confine for Recording {
        fn confine(&self, command: &Command, scratch: &Path) -> Result<Confined, String> {
            let mut line = format!("echo read>record& {}", command.program.display());
            for arg in &command.args {
                line.push(' ');
                line.push_str(&arg.to_string_lossy());
            }
            Ok(Confined {
                program: system().join("cmd.exe"),
                args: vec!["/d".into(), "/c".into(), line.into()],
                env: command.env.clone(),
                dir: scratch.to_owned(),
            })
        }

        fn traced(&self, _: &Command, scratch: &Path) -> Result<Traced, String> {
            match std::fs::read(scratch.join("record")) {
                Ok(record) if record.trim_ascii() == b"read" => Ok(Traced {
                    files: vec![(PathBuf::from(r"C:\Windows\win.ini"), None)],
                    ..Traced::default()
                }),
                other => Err(format!("no record: {other:?}")),
            }
        }
    }

    #[test]
    fn a_sandboxed_process_runs_as_its_confine_says_and_its_trace_comes_back() {
        let root = dir("confine");
        let mut driver = Iocp::new(&root, 1).unwrap().confined(Arc::new(Recording));
        let mut traced = command("cmd.exe", &["/d", "/c", "echo hi"], b"");
        traced.sandbox = Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: true };
        let seen = run(&mut driver, vec![Effect::Spawn { command: traced, tag: Tag(1) }]);
        let exit = exit(&seen[&1]);
        assert_eq!((exit.code, exit.stdout.as_slice()), (Some(0), &b"hi\r\n"[..]));
        let wanted = Traced { files: vec![(PathBuf::from(r"C:\Windows\win.ini"), None)], ..Traced::default() };
        assert_eq!(exit.traced, Some(wanted));
        let left = std::fs::read_dir(root.join(".blaze-spawn")).unwrap().count();
        assert_eq!(left, 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Listens, connects, sends each way, hangs up each end and stops listening.
    fn round_trip(root: &Path, at: Address) {
        let mut driver = Iocp::new(root, 1).unwrap();
        let mut effects = vec![Effect::Listen { at: at.clone(), tag: Tag(1) }, Effect::Connect { at, tag: Tag(2) }];
        let (mut server, mut client) = (None, None);
        let mut ends = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, effects| match (done.tag.0, done.done) {
            (1, Done::Accepted { conn }) => {
                server = Some(conn);
                effects.push(Effect::Recv { conn, most: 64, tag: Tag(3) });
            }
            (2, Done::Connected { conn }) => {
                client = Some(conn);
                effects.push(Effect::Send { conn, bytes: b"ping".to_vec(), tag: Tag(4) });
            }
            (3, Done::Read(bytes)) => {
                assert_eq!(bytes, b"ping");
                let (server, client) = (server.unwrap(), client.unwrap());
                effects.push(Effect::Send { conn: server, bytes: b"pong".to_vec(), tag: Tag(5) });
                effects.push(Effect::Recv { conn: client, most: 64, tag: Tag(6) });
            }
            (6, Done::Read(bytes)) => {
                assert_eq!(bytes, b"pong");
                effects.push(Effect::Hangup { conn: client.unwrap(), tag: Tag(7) });
                effects.push(Effect::Recv { conn: server.unwrap(), most: 64, tag: Tag(8) });
            }
            (8, Done::Read(bytes)) => {
                assert!(bytes.is_empty(), "{bytes:?}");
                effects.push(Effect::Hangup { conn: server.unwrap(), tag: Tag(9) });
                effects.push(Effect::Unlisten { tag: Tag(1) });
            }
            (tag @ (4 | 5), Done::Sent) | (tag @ (7 | 9), Done::HungUp) | (tag @ 1, Done::Unlistened) => {
                ends.push(tag)
            }
            (tag, other) => panic!("{tag}: {other:?}"),
        });
        ends.sort();
        assert_eq!(ends, [1, 4, 5, 7, 9]);
        assert_ne!(server, client);
        assert_eq!(driver.in_flight(), 0);
    }

    #[test]
    fn a_unix_socket_round_trip() {
        let root = dir("unix");
        round_trip(&root, Address::Unix(name("s")));
        assert!(!root.join("s").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_unix_socket_someone_listens_on_is_refused() {
        let root = dir("taken");
        let mut driver = Iocp::new(&root, 1).unwrap();
        let mut out = Vec::new();
        driver.submit(&mut vec![Effect::Listen { at: Address::Unix(name("s")), tag: Tag(1) }]);
        driver.poll(&mut out, Duration::ZERO);
        assert!(out.is_empty(), "{out:?}");
        let seen = run(&mut driver, vec![
            Effect::Listen { at: Address::Unix(name("s")), tag: Tag(2) },
            Effect::Unlisten { tag: Tag(1) },
        ]);
        assert!(matches!(&seen[&2], Done::Failed(why) if why.0.contains("listens")), "{seen:?}");
        assert_eq!(seen[&1], Done::Unlistened);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_tcp_round_trip() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let at = free.local_addr().unwrap();
        drop(free);
        let root = dir("tcp");
        round_trip(&root, Address::Tcp(at));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn what_names_nothing_fails() {
        let root = dir("nothing");
        let mut driver = Iocp::new(&root, 1).unwrap();
        let seen = run(&mut driver, vec![
            Effect::Unwatch { tag: Tag(1) },
            Effect::Unlisten { tag: Tag(2) },
            Effect::Recv { conn: Conn(77), most: 8, tag: Tag(3) },
            Effect::Hangup { conn: Conn(77), tag: Tag(4) },
            Effect::Connect { at: Address::Unix(name("nobody")), tag: Tag(5) },
            Effect::Kill { tag: Tag(6) },
        ]);
        assert_eq!(seen.keys().copied().collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
        assert!(seen.values().all(|it| matches!(it, Done::Failed(_))), "{seen:?}");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn timers_fire_in_their_order() {
        let mut driver = Iocp::new(&dir("timers"), 1).unwrap();
        let mut effects: Vec<Effect> = (0..3)
            .map(|n| Effect::Timer { after: Duration::from_millis(10 * (3 - n)), tag: Tag(n) })
            .collect();
        effects.push(Effect::Timer { after: Duration::ZERO, tag: Tag(3) });
        let mut fired = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| {
            assert_eq!(done.done, Done::Fired);
            fired.push(done.tag.0);
        });
        assert_eq!(fired, [3, 2, 1, 0]);
    }

    #[test]
    fn a_wait_ends_at_its_time() {
        let mut driver = Iocp::new(&dir("wait"), 1).unwrap();
        driver.submit(&mut vec![Effect::Timer { after: Duration::from_secs(60), tag: Tag(1) }]);
        let began = Instant::now();
        let mut out = Vec::new();
        assert_eq!(driver.poll(&mut out, Duration::from_millis(20)), 0);
        let waited = began.elapsed();
        assert!(waited >= Duration::from_millis(19) && waited < Duration::from_secs(5), "{waited:?}");
        assert_eq!(driver.in_flight(), 1);
    }

    #[test]
    fn a_mapping_is_the_file() {
        let root = dir("map");
        let mut driver = Iocp::new(&root, 2).unwrap();
        let file = name("store/pack");
        let seen = run(&mut driver, vec![Effect::Write { file: file.clone(), bytes: b"packed".to_vec(), tag: Tag(1) }]);
        assert_eq!(seen[&1], Done::Written);
        let seen = run(&mut driver, vec![
            Effect::Map { file, tag: Tag(2) },
            Effect::Map { file: name("missing"), tag: Tag(3) },
        ]);
        match &seen[&2] {
            Done::Mapped(mapping) => assert_eq!(mapping.bytes(), b"packed"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(seen[&3], Done::Failed(_)));
        drop(seen);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The file effects, one after another, end as on the threads driver (a mode Windows keeps
    /// is asked for after the script, alone).
    #[test]
    fn files_end_as_on_threads() {
        let file = name("vfs/journal");
        fn script(driver: &mut impl Driver, file: &FileName) -> Vec<String> {
            let mut effects = vec![Effect::Write { file: file.clone(), bytes: b"one".to_vec(), tag: Tag(1) }];
            let mut seen = Vec::new();
            run_until_idle(driver, &mut effects, |done, effects| {
                seen.push(match done.done {
                    Done::Stat(Some(meta)) => format!("stat {} {}", meta.len, meta.dir),
                    Done::Listed(entries) => {
                        let entries: Vec<_> =
                            entries.iter().map(|(name, meta)| (name.as_str(), meta.len, meta.dir)).collect();
                        format!("listed {entries:?}")
                    }
                    Done::Failed(_) => "failed".to_owned(),
                    other => format!("{other:?}"),
                });
                let next = match done.tag.0 {
                    1 => Effect::Append { file: file.clone(), bytes: b"two".to_vec(), tag: Tag(2) },
                    2 => Effect::Sync { file: file.clone(), tag: Tag(3) },
                    3 => Effect::Read { file: file.clone(), at: 2, len: 3, tag: Tag(4) },
                    4 => Effect::Truncate { file: file.clone(), len: 4, tag: Tag(5) },
                    5 => Effect::ReadAll { file: file.clone(), tag: Tag(6) },
                    6 => Effect::Stat { file: file.clone(), tag: Tag(7) },
                    7 => Effect::Stat { file: name("vfs"), tag: Tag(8) },
                    8 => Effect::Clone { from: file.clone(), to: name("out/c"), tag: Tag(9) },
                    9 => Effect::Link { from: file.clone(), to: name("out/l"), tag: Tag(10) },
                    10 => Effect::ReadAll { file: name("out/l"), tag: Tag(11) },
                    11 => Effect::Remove { file: name("out/l"), tag: Tag(12) },
                    12 => Effect::Stat { file: name("out/l"), tag: Tag(13) },
                    13 => Effect::ReadAll { file: name("missing"), tag: Tag(14) },
                    14 => Effect::Read { file: name("missing"), at: 0, len: 1, tag: Tag(15) },
                    15 => Effect::WriteAt { file: file.clone(), at: 6, bytes: b"!".to_vec(), tag: Tag(16) },
                    16 => Effect::ReadAll { file: file.clone(), tag: Tag(17) },
                    17 => Effect::Mode { file: file.clone(), mode: 0o600, tag: Tag(18) },
                    18 => Effect::Rename { from: file.clone(), to: name("vfs/moved"), tag: Tag(19) },
                    19 => Effect::List { dir: name("vfs"), tag: Tag(20) },
                    20 => Effect::List { dir: name("missing"), tag: Tag(21) },
                    21 => Effect::WriteAt { file: name("missing"), at: 0, bytes: vec![1], tag: Tag(22) },
                    _ => return,
                };
                effects.push(next);
            });
            seen
        }
        let (ours, theirs) = (dir("files"), dir("files-threads"));
        let mut iocp = Iocp::new(&ours, 2).unwrap();
        let ran = script(&mut iocp, &file);
        let threads = script(&mut Threads::new(&theirs, 2), &file);
        // Shared blocks on ReFS; elsewhere an error naming the file system.
        assert!(ran[8] == "failed" || ran[8] == format!("{:?}", Done::Cloned), "{}", ran[8]);
        assert_eq!(ran, threads);
        assert_eq!(ran[5], format!("{:?}", Done::Read(b"onet".to_vec())));
        assert_eq!(ran[3], format!("{:?}", Done::Read(b"etw".to_vec())));
        assert_eq!(ran[6], "stat 4 false");
        assert!(ran[7].starts_with("stat ") && ran[7].ends_with(" true"), "{}", ran[7]);
        assert_eq!(ran[14], "failed");
        assert_eq!(ran[16], format!("{:?}", Done::Read(b"onet\0\0!".to_vec())));
        // 0o600 asks others to read nothing, which a file's attributes cannot say.
        assert_eq!(ran[17], "failed");
        assert_eq!(ran[18], format!("{:?}", Done::Renamed));
        assert!(ran[19].starts_with("listed ") && ran[19].contains("moved"), "{}", ran[19]);
        assert!(!ran[19].contains("journal"), "{}", ran[19]);
        assert_eq!((ran[20].as_str(), ran[21].as_str()), ("failed", "failed"));
        let moved = name("vfs/moved");
        let seen = run(&mut iocp, vec![
            Effect::Mode { file: moved.clone(), mode: 0o555, tag: Tag(1) },
            Effect::Stat { file: moved.clone(), tag: Tag(2) },
        ]);
        assert_eq!(seen[&1], Done::Moded);
        assert!(std::fs::metadata(ours.join("vfs/moved")).unwrap().permissions().readonly());
        let seen = run(&mut iocp, vec![Effect::Mode { file: moved, mode: 0o755, tag: Tag(3) }]);
        assert_eq!(seen[&3], Done::Moded);
        assert!(!std::fs::metadata(ours.join("vfs/moved")).unwrap().permissions().readonly());
        drop(iocp);
        for it in [ours, theirs] {
            std::fs::remove_dir_all(it).unwrap();
        }
    }

    #[test]
    fn a_watch_hears_a_file_written_under_it_and_nothing_outside_then_ends() {
        let root = dir("watch");
        let mut driver = Iocp::new(&root, 1).unwrap();
        run(&mut driver, vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }]);
        driver.submit(&mut vec![Effect::Watch { dir: name("src"), tag: Tag(7) }]);
        let mut heard = Vec::new();
        let mut asked_end = false;
        let mut ended = false;
        let mut done = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut n = 0;
        while !ended && Instant::now() < deadline {
            if !asked_end {
                n += 1;
                std::fs::write(root.join("outside"), b"not watched").unwrap();
                driver.submit(&mut vec![Effect::Write {
                    file: name("src/deep/lib.rs"),
                    bytes: b"fn f() {}".to_vec(),
                    tag: Tag(100 + n),
                }]);
            }
            driver.poll(&mut done, Duration::from_millis(50));
            for it in done.drain(..) {
                match it.done {
                    Done::Changed { files, .. } => {
                        assert!(!ended, "a change after the end");
                        heard.extend(files)
                    }
                    Done::Unwatched => ended = true,
                    Done::Written => {}
                    other => panic!("{other:?}"),
                }
            }
            if !asked_end && heard.contains(&name("src/deep/lib.rs")) {
                asked_end = true;
                driver.submit(&mut vec![Effect::Unwatch { tag: Tag(7) }]);
            }
        }
        assert!(ended, "the watch never ended; heard {heard:?}");
        assert!(heard.iter().all(|it| it.is_under(&name("src"))), "{heard:?}");
        run(&mut driver, Vec::new());
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The batches of a journal of `dir` from `since` until `enough` says so of them (or 20
    /// seconds pass), then its unwatch.
    fn journal(driver: &mut Iocp, dir: &str, since: Cursor, enough: impl Fn(&[Journaled]) -> bool) -> Vec<Journaled> {
        driver.submit(&mut vec![Effect::Journal { dir: name(dir), since, tag: Tag(7) }]);
        let mut batches = Vec::new();
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut asked_end = false;
        loop {
            driver.poll(&mut out, Duration::from_millis(50));
            for it in out.drain(..) {
                assert_eq!(it.tag, Tag(7));
                match it.done {
                    Done::Journaled(batch) => batches.push(*batch),
                    Done::Unwatched => {
                        assert_eq!(driver.in_flight(), 0);
                        return batches;
                    }
                    other => panic!("{other:?} after {batches:?}"),
                }
            }
            if !asked_end && (enough(&batches) || Instant::now() >= deadline) {
                asked_end = true;
                driver.submit(&mut vec![Effect::Unwatch { tag: Tag(7) }]);
            }
            assert!(Instant::now() < deadline + Duration::from_secs(10), "the journal never ended: {batches:?}");
        }
    }

    fn usn(cursor: Cursor) -> (u64, u64, i64) {
        match cursor {
            Cursor::Usn { volume, journal, next } => (volume, journal, next),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_journal_from_its_cursor_hears_what_changed_while_it_was_unwatched() {
        let root = dir("journal");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut driver = Iocp::new(&root, 2).unwrap();
        let first = journal(&mut driver, "src", Cursor::None, |it| !it.is_empty());
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].caught_up && first[0].files.is_empty() && first[0].lost.is_empty(), "{first:?}");
        let saved = first[0].cursor;
        let (volume, id, at) = usn(saved);
        std::fs::write(src.join("a.rs"), b"fn a() {}").unwrap();
        std::fs::rename(src.join("a.rs"), src.join("b.rs")).unwrap();
        std::fs::write(src.join("gone.rs"), b"").unwrap();
        std::fs::remove_file(src.join("gone.rs")).unwrap();
        std::fs::write(root.join("outside.rs"), b"not watched").unwrap();
        // A directory made, filled and renamed: its file is named where it is now.
        std::fs::create_dir(src.join("new")).unwrap();
        std::fs::write(src.join("new/f.rs"), b"fn f() {}").unwrap();
        std::fs::rename(src.join("new"), src.join("moved")).unwrap();
        let batches = journal(&mut driver, "src", saved, |it| it.iter().any(|it| it.caught_up));
        let caught = batches.iter().position(|it| it.caught_up).expect("caught up");
        assert!(batches[..caught].iter().all(|it| !it.caught_up));
        let files: Vec<&str> = batches.iter().flat_map(|it| &it.files).map(FileName::as_str).collect();
        for wanted in ["src/b.rs", "src/moved", "src/moved/f.rs"] {
            assert!(files.contains(&wanted), "{wanted} in {files:?}");
        }
        assert!(files.iter().all(|it| it.starts_with("src")), "{files:?}");
        // Unprivileged records have no names: a removal or an old name is its directory's.
        let lost: Vec<_> = batches.iter().flat_map(|it| it.lost.iter().cloned()).collect();
        assert!(lost.contains(&(name("src"), Lost::Unnamed)), "{lost:?}");
        assert!(lost.iter().all(|(_, why)| *why == Lost::Unnamed), "{lost:?}");
        let (volume_now, id_now, next) = usn(batches.last().unwrap().cursor);
        assert_eq!((volume_now, id_now), (volume, id));
        assert!(next > at);
        let cursors: Vec<i64> = batches.iter().map(|it| usn(it.cursor).2).collect();
        assert!(cursors.windows(2).all(|it| it[0] <= it[1]), "{cursors:?}");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_cursor_of_another_journal_is_a_reset() {
        let root = dir("journal-reset");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut driver = Iocp::new(&root, 1).unwrap();
        let first = journal(&mut driver, "src", Cursor::None, |it| !it.is_empty());
        let (volume, id, next) = usn(first[0].cursor);
        for since in [
            Cursor::Usn { volume, journal: id.wrapping_add(1), next },
            Cursor::Usn { volume: volume ^ 1, journal: id, next },
            Cursor::Sim { journal: 1, next: 2 },
        ] {
            let batches = journal(&mut driver, "src", since, |it| !it.is_empty());
            assert!(batches[0].caught_up, "{batches:?}");
            assert_eq!(batches[0].lost, [(name("src"), Lost::Reset)]);
            let (volume_now, id_now, _) = usn(batches[0].cursor);
            assert_eq!((volume_now, id_now), (volume, id));
        }
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stamps_of_a_file_a_directory_a_missing_name_and_links() {
        let root = dir("stamps");
        std::fs::create_dir_all(root.join("a/d")).unwrap();
        std::fs::write(root.join("a/f"), b"hello").unwrap();
        // A junction needs no privilege; a symbolic link needs developer mode or elevation.
        let made = std::process::Command::new(system().join("cmd.exe"))
            .args(["/d", "/c", "mklink", "/J"])
            .arg(root.join("a").join("j"))
            .arg(root.join("a").join("d"))
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        let symlink = std::os::windows::fs::symlink_file(root.join("a").join("f"), root.join("a").join("s")).is_ok();
        let mut names = Names::default();
        for it in ["a/f", "a/d", "a/missing", "a/missing/deeper", "a/j", "a/s"] {
            assert!(names.push_str(it));
        }
        let stamps = stamps(&root, &names).unwrap();
        let (file, dir_, missing, deeper, junction, link) =
            (stamps[0], stamps[1], stamps[2], stamps[3], stamps[4], stamps[5]);
        assert!(file.is_file() && file.size == 5 && file.ino != 0 && file.mtime_ns != 0, "{file:?}");
        assert_eq!(file.mode, 0o100644);
        assert!(dir_.is_dir() && dir_.mode == 0o040755, "{dir_:?}");
        assert_eq!((missing, deeper), (Stamp::GONE, Stamp::GONE));
        assert!(junction.is_link() && junction.mode == 0o120777 && junction.ino != dir_.ino, "{junction:?}");
        match symlink {
            true => assert!(link.is_link() && link.ino != file.ino, "{link:?}"),
            false => assert_eq!(link, Stamp::GONE),
        }
        let mut driver = Iocp::new(&root, 2).unwrap();
        let seen = run(&mut driver, vec![
            Effect::Stamp { files: Arc::new(names.clone()), tag: Tag(1) },
            Effect::Entries { dir: name("a"), tag: Tag(2) },
            Effect::Entries { dir: name("a/missing"), tag: Tag(3) },
            Effect::Entries { dir: name("a/f"), tag: Tag(4) },
        ]);
        assert_eq!(seen[&1], Done::Stamped(stamps.clone()));
        // The listing's stamps are the ones a handle of each gives.
        let mut wanted = vec![(name("a/d"), dir_), (name("a/f"), file), (name("a/j"), junction)];
        if symlink {
            wanted.push((name("a/s"), link));
        }
        assert_eq!(seen[&2], Done::Entries(wanted));
        assert!(matches!(seen[&3], Done::Failed(_)) && matches!(seen[&4], Done::Failed(_)), "{seen:?}");
        // A write moves the file's stamp and no other.
        std::fs::write(root.join("a/f"), b"hello, again").unwrap();
        let again = super::stamps(&root, &names).unwrap();
        assert!(again[0].moved(&file) && again[0].ino == file.ino && again[0].size == 12);
        assert_eq!(&again[1..5], &stamps[1..5]);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_flush_fails_naming_why() {
        let root = dir("flush");
        let mut driver = Iocp::new(&root, 1).unwrap();
        let seen = run(&mut driver, vec![Effect::Flush { watch: Tag(7), tag: Tag(8) }]);
        assert!(matches!(&seen[&8], Done::Failed(why) if why.0.contains("no barrier")), "{seen:?}");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The slab of requests answers as the map of serial ids it replaces, over random makes,
    /// ends and lookups, a lookup by an ended request's handle included; and the id a head
    /// carries, which is all a packet hands back, is its request's handle.
    #[test]
    fn requests_agree_with_the_map_they_replace() {
        struct Model {
            tag: Tag,
            head: Head,
        }
        type Of = blaze_mem::Handle<Model>;
        for seed in 0..64 {
            let mut rng = crate::sim::Rng::new(seed);
            let mut slab: Slab<Model> = Slab::new(1 << 12, MemTag::Io).unwrap();
            let mut map: FxHashMap<u64, Tag> = FxHashMap::default();
            let mut next = 0u64;
            let (mut live, mut ended): (Vec<(u64, Of)>, Vec<(u64, Of)>) = (Vec::new(), Vec::new());
            for _ in 0..2000 {
                match rng.below(4) {
                    0 | 1 if live.len() < 1000 => {
                        let tag = Tag(rng.next_u64());
                        let handle = slab.insert_with(|handle| Model { tag, head: Head::new(handle.to_bits()) });
                        assert!(map.insert(next, tag).is_none());
                        live.push((next, handle));
                        next += 1;
                    }
                    2 if !live.is_empty() => {
                        let (id, handle) = live.swap_remove(rng.below(live.len() as u64) as usize);
                        assert_eq!(slab.remove(handle).map(|it| it.tag), map.remove(&id));
                        ended.push((id, handle));
                    }
                    _ if !live.is_empty() => {
                        let (id, handle) = live[rng.below(live.len() as u64) as usize];
                        let model = slab.get(handle).expect("a live request");
                        assert_eq!(Some(&model.tag), map.get(&id));
                        // SAFETY: the head of a model in the slab, which is alive.
                        let bits = unsafe { Head::id_of(model.head.overlapped()) };
                        assert_eq!(Of::from_bits(bits), handle);
                    }
                    _ => {}
                }
                if !ended.is_empty() {
                    let (id, handle) = ended[rng.below(ended.len() as u64) as usize];
                    assert!(slab.get(handle).is_none() && !slab.contains(handle));
                    assert!(!map.contains_key(&id));
                }
                assert_eq!(slab.len(), map.len());
            }
        }
    }

    /// A packet of a request that has ended, come after its slot was given to another, names
    /// the ended one: it is dropped and counted, and the request now in the slot goes on.
    #[test]
    fn a_packet_of_a_request_ended_and_its_slot_reused_is_stale() {
        let root = dir("stale");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut driver = Iocp::new(&root, 1).unwrap();
        driver.submit(&mut vec![Effect::Watch { dir: name("src"), tag: Tag(1) }]);
        let ended = driver.watches[&Tag(1)];
        let seen = run(&mut driver, vec![Effect::Unwatch { tag: Tag(1) }]);
        assert_eq!(seen[&1], Done::Unwatched);
        assert!(!driver.requests.contains(ended));
        driver.submit(&mut vec![Effect::Watch { dir: name("src"), tag: Tag(2) }]);
        let now = driver.watches[&Tag(2)];
        assert_eq!(now.index(), ended.index(), "the ended request's slot is reused");
        assert_ne!(now, ended);
        // The ended request's completion as the port hands it back had it come late: through
        // an `OVERLAPPED` whose head carries the ended request's handle.
        let late = Head::new(ended.to_bits());
        driver.port.post(KEY_IO, 0, late.overlapped() as usize).unwrap();
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while driver.stale() == 0 && Instant::now() < deadline {
            driver.poll(&mut out, Duration::from_millis(10));
        }
        assert_eq!(driver.stale(), 1);
        assert!(out.iter().all(|it| matches!(it.done, Done::Changed { .. })), "{out:?}");
        assert!(driver.requests.contains(now));
        let seen = run(&mut driver, vec![Effect::Unwatch { tag: Tag(2) }]);
        assert_eq!(seen[&2], Done::Unwatched);
        assert_eq!(driver.stale(), 1);
        drop(driver);
        drop(late);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Polls until `out` holds `n` completions.
    fn until(driver: &mut Iocp, out: &mut Vec<Completion>, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while out.len() < n {
            assert!(Instant::now() < deadline, "{out:?}");
            driver.poll(out, Duration::from_millis(100));
        }
    }

    /// A warm sync, stat and read of a file whose handle is kept: the read's bytes are the
    /// one allocation on the polling thread (the sync and stat run on the driver's threads).
    #[test]
    fn a_warm_sync_stat_and_read_allocate_only_the_read() {
        let root = dir("budget-files");
        std::fs::write(root.join("f"), b"hello").unwrap();
        let mut driver = Iocp::new(&root, 2).unwrap();
        let file = name("f");
        let mut effects = Vec::with_capacity(8);
        let mut out = Vec::with_capacity(64);
        for round in 0..4 {
            let mut asked = vec![
                Effect::Sync { file: file.clone(), tag: Tag(1) },
                Effect::Stat { file: file.clone(), tag: Tag(2) },
                Effect::Read { file: file.clone(), at: 0, len: 5, tag: Tag(3) },
            ];
            out.clear();
            let ((), used) = blaze_alloc_count::measure(|| {
                effects.append(&mut asked);
                driver.submit(&mut effects);
                until(&mut driver, &mut out, 3);
            });
            assert!(out.iter().all(|it| !matches!(it.done, Done::Failed(_))), "{out:?}");
            let read = out.iter().find(|it| it.tag == Tag(3)).unwrap();
            assert_eq!(read.done, Done::Read(b"hello".to_vec()));
            if round > 0 {
                assert!(
                    used.allocations <= 1,
                    "a warm sync, stat and read made {} allocations; the read's bytes are one",
                    used.allocations
                );
            }
        }
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_warm_timer_allocates_nothing() {
        let mut driver = Iocp::new(&dir("budget-timer"), 1).unwrap();
        let mut effects = Vec::with_capacity(4);
        let mut out = Vec::with_capacity(16);
        for round in 0..4 {
            out.clear();
            let ((), used) = blaze_alloc_count::measure(|| {
                effects.push(Effect::Timer { after: Duration::ZERO, tag: Tag(round) });
                driver.submit(&mut effects);
                until(&mut driver, &mut out, 1);
            });
            assert_eq!(out[0].done, Done::Fired);
            if round > 0 {
                assert_eq!(used.allocations, 0, "a warm timer's arm and fire");
            }
        }
    }

    /// The allocations of what `out` carries of a watch's changes: each `Done::Changed`'s list
    /// and the names in it.
    fn changes(out: &[Completion]) -> u64 {
        out.iter()
            .map(|it| match &it.done {
                Done::Changed { files, .. } => u64::from(files.capacity() > 0) + files.len() as u64,
                _ => 0,
            })
            .sum()
    }

    /// A warm watch added, told of a file written under it, and removed: what it allocates on
    /// the polling thread is what its changes carry.
    #[test]
    fn a_warm_watch_allocates_only_its_changes() {
        let root = dir("budget-watch");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut driver = Iocp::new(&root, 1).unwrap();
        let mut effects = Vec::with_capacity(4);
        let mut out = Vec::with_capacity(64);
        let wanted = name("src/f.rs");
        for round in 0..4u64 {
            let (watch, unwatch) = (Effect::Watch { dir: name("src"), tag: Tag(1) }, Effect::Unwatch { tag: Tag(1) });
            out.clear();
            let ((), added) = blaze_alloc_count::measure(|| {
                effects.push(watch);
                driver.submit(&mut effects);
                driver.poll(&mut out, Duration::ZERO);
            });
            std::fs::write(root.join("src/f.rs"), round.to_ne_bytes()).unwrap();
            let mut used = added.allocations;
            let deadline = Instant::now() + Duration::from_secs(10);
            let heard = |out: &[Completion]| {
                out.iter().any(|it| matches!(&it.done, Done::Changed { files, .. } if files.contains(&wanted)))
            };
            while !heard(&out) {
                assert!(Instant::now() < deadline, "{out:?}");
                let (_, polled) = blaze_alloc_count::measure(|| driver.poll(&mut out, Duration::from_millis(50)));
                used += polled.allocations;
            }
            let ((), removed) = blaze_alloc_count::measure(|| {
                effects.push(unwatch);
                driver.submit(&mut effects);
                while !out.iter().any(|it| it.done == Done::Unwatched) {
                    assert!(Instant::now() < deadline + Duration::from_secs(10), "{out:?}");
                    driver.poll(&mut out, Duration::from_millis(50));
                }
            });
            used += removed.allocations;
            assert!(out.iter().all(|it| matches!(it.done, Done::Changed { .. } | Done::Unwatched)), "{out:?}");
            if round > 0 {
                let payload = changes(&out);
                assert!(used <= payload, "a warm watch made {used} allocations; its changes carry {payload}");
            }
        }
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A warm send and receive on a connection: the bytes received are the one allocation.
    #[test]
    fn a_warm_connection_sends_and_receives_only_its_bytes() {
        let root = dir("budget-conn");
        let mut driver = Iocp::new(&root, 1).unwrap();
        let mut out = Vec::with_capacity(16);
        driver.submit(&mut vec![Effect::Listen { at: Address::Tcp("127.0.0.1:0".parse().unwrap()), tag: Tag(1) }]);
        let at = driver.local_addr(Tag(1)).unwrap();
        driver.submit(&mut vec![Effect::Connect { at: Address::Tcp(at), tag: Tag(2) }]);
        until(&mut driver, &mut out, 2);
        let (mut server, mut client) = (None, None);
        for it in out.drain(..) {
            match it.done {
                Done::Accepted { conn } => server = Some(conn),
                Done::Connected { conn } => client = Some(conn),
                other => panic!("{other:?}"),
            }
        }
        let (server, client) = (server.unwrap(), client.unwrap());
        let mut effects = Vec::with_capacity(4);
        for round in 0..4 {
            let send = Effect::Send { conn: client, bytes: b"ping".to_vec(), tag: Tag(3) };
            let recv = Effect::Recv { conn: server, most: 64, tag: Tag(4) };
            out.clear();
            let ((), used) = blaze_alloc_count::measure(|| {
                effects.push(send);
                effects.push(recv);
                driver.submit(&mut effects);
                until(&mut driver, &mut out, 2);
            });
            let mut ends: Vec<_> = out.iter().map(|it| (it.tag.0, it.done.clone())).collect();
            ends.sort_by_key(|it| it.0);
            assert_eq!(ends, [(3, Done::Sent), (4, Done::Read(b"ping".to_vec()))]);
            if round > 0 {
                assert!(
                    used.allocations <= 1,
                    "a warm send and receive made {} allocations; the bytes received are one",
                    used.allocations
                );
            }
        }
        let seen = run(&mut driver, vec![
            Effect::Hangup { conn: client, tag: Tag(5) },
            Effect::Hangup { conn: server, tag: Tag(6) },
            Effect::Unlisten { tag: Tag(1) },
        ]);
        assert_eq!((&seen[&5], &seen[&6], &seen[&1]), (&Done::HungUp, &Done::HungUp, &Done::Unlistened));
        assert!(driver.conns.is_empty(), "a hung-up connection is closed once nothing is on it");
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }
}
