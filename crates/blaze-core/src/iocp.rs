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
//! The rule every request keeps (see [`Request`]): what the kernel reads or writes for it (its
//! `OVERLAPPED`, its bytes, the handle it is on) is the request's, and the request is dropped
//! only once its completion is taken from the port, never before.

pub(crate) mod files;
mod net;
mod process;
mod sys;
mod watch;

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, VecDeque},
    io,
    os::windows::io::{AsRawHandle as _, OwnedHandle, OwnedSocket},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use rustc_hash::FxHashMap;
use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_CONNECTION_ABORTED, ERROR_HANDLE_EOF, ERROR_NETNAME_DELETED,
        ERROR_NO_DATA, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, HANDLE,
    },
    Networking::WinSock::WSAECONNRESET,
    Storage::FileSystem::{ReadFile, WriteFile},
    System::{
        IO::{CancelIoEx, OVERLAPPED_ENTRY},
        SystemServices::JOB_OBJECT_MSG_JOB_MEMORY_LIMIT,
        Threading::WaitForSingleObject,
    },
};

use self::sys::{KEY_EXIT, KEY_IO, KEY_JOB, KEY_WAKE, Port, Slot};
use crate::{Address, Command, Completion, Confine, Conn, Done, Driver, Effect, FileName, IoError, Tag};

/// Packets taken from the port at once.
const ENTRIES: usize = 64;
/// What one read of a process's output takes, and the most one receive allocates.
const CHUNK: usize = 64 << 10;
/// Handles kept open for reading and appending, by name; past this many they are all let go.
const CACHED: usize = 256;
/// How long dropping the driver waits for the port to give back what it was lent.
const GIVE_BACK: Duration = Duration::from_secs(5);

/// What a request in flight is doing.
enum Op {
    /// Into `bytes` from `at`: all of them, or to the end of the file where `to_end`, the
    /// buffer growing.
    Read { handle: Arc<OwnedHandle>, bytes: Vec<u8>, at: u64, filled: usize, to_end: bool },
    /// `bytes` written at `at`, or at the end where `None` (an append).
    Write { handle: Arc<OwnedHandle>, bytes: Vec<u8>, at: Option<u64>, put: usize },
    Watch(watch::Watching),
    /// A process's output, `err` its standard error.
    Output { spawn: Tag, err: bool, pipe: OwnedHandle, chunk: Box<[u8]> },
    Input { spawn: Tag, pipe: OwnedHandle, bytes: Vec<u8>, put: usize },
    Accept(net::Accepting),
    Connect { connecting: net::Connecting, at: Address },
    Recv { conn: Conn, socket: Arc<OwnedSocket>, bytes: Vec<u8>, flags: Box<u32> },
    Send { conn: Conn, socket: Arc<OwnedSocket>, bytes: Vec<u8>, put: usize },
}

impl Op {
    /// What the request is on: what cancelling it names.
    fn handle(&self) -> HANDLE {
        match self {
            Op::Read { handle, .. } | Op::Write { handle, .. } => handle.as_raw_handle(),
            Op::Watch(watching) => watching.raw(),
            Op::Output { pipe, .. } | Op::Input { pipe, .. } => pipe.as_raw_handle(),
            Op::Accept(accepting) => net::raw(&accepting.listener.socket) as HANDLE,
            Op::Connect { connecting, .. } => net::raw(&connecting.socket) as HANDLE,
            Op::Recv { socket, .. } | Op::Send { socket, .. } => net::raw(socket) as HANDLE,
        }
    }
}

/// An overlapped request: its slot, whose `OVERLAPPED` the kernel holds while it is in flight,
/// and its op, which owns the bytes and the handle. While in flight it is in
/// [`Iocp::requests`], which lets it go only once its completion is taken, and moving it moves
/// neither the slot (boxed) nor its bytes (on the heap).
struct Request {
    tag: Tag,
    slot: Box<Slot>,
    op: Op,
}

impl Request {
    /// Asks the kernel for the op's next step. Ok: its completion comes to the port.
    fn ask(&mut self) -> io::Result<()> {
        let Request { slot, op, .. } = self;
        match op {
            Op::Read { handle, bytes, at, filled, .. } => {
                let rest = &mut bytes[*filled..];
                let overlapped = slot.reset(*at + *filled as u64);
                // SAFETY: an open overlapped handle, and bytes and an `OVERLAPPED` that the
                // request keeps until the completion is taken (see `Request`).
                sys::pending(unsafe {
                    ReadFile(handle.as_raw_handle(), rest.as_mut_ptr(), len(rest), std::ptr::null_mut(), overlapped)
                })
            }
            Op::Write { handle, bytes, at, put } => {
                let rest = &bytes[*put..];
                // All ones: at the end of the file, wherever that is when it is written.
                let overlapped = slot.reset(at.map_or(u64::MAX, |at| at + *put as u64));
                // SAFETY: as for a read; the bytes are only read.
                sys::pending(unsafe {
                    WriteFile(handle.as_raw_handle(), rest.as_ptr(), len(rest), std::ptr::null_mut(), overlapped)
                })
            }
            Op::Watch(watching) => watching.read(slot.reset(0)),
            Op::Output { pipe, chunk, .. } => {
                let overlapped = slot.reset(0);
                // SAFETY: as for a read.
                sys::pending(unsafe {
                    ReadFile(pipe.as_raw_handle(), chunk.as_mut_ptr(), len(chunk), std::ptr::null_mut(), overlapped)
                })
            }
            Op::Input { pipe, bytes, put, .. } => {
                let rest = &bytes[*put..];
                let overlapped = slot.reset(0);
                // SAFETY: as for a write.
                sys::pending(unsafe {
                    WriteFile(pipe.as_raw_handle(), rest.as_ptr(), len(rest), std::ptr::null_mut(), overlapped)
                })
            }
            Op::Accept(accepting) => accepting.accept(slot.reset(0)),
            Op::Recv { socket, bytes, flags, .. } => net::recv(net::raw(socket), bytes, flags, slot.reset(0)),
            Op::Send { socket, bytes, put, .. } => net::send(net::raw(socket), &bytes[*put..], slot.reset(0)),
            Op::Connect { .. } => unreachable!("a connect is asked for once, as it is made"),
        }
    }

    /// Asks the kernel to end it early; its completion still comes, as cancelled or as it
    /// ended before.
    fn cancel(&mut self) {
        let handle = self.op.handle();
        // SAFETY: the handle the request is on and its `OVERLAPPED`, both alive (the
        // request's). Failing (`ERROR_NOT_FOUND`) means it has ended already.
        unsafe { CancelIoEx(handle, &raw mut self.slot.overlapped) };
    }
}

fn len(bytes: &[u8]) -> u32 {
    bytes.len().min(u32::MAX as usize) as u32
}

/// What the driver's threads send back.
enum Back {
    Done(Completion),
    /// A Unix socket's connect, made with a blocking call (see `net`).
    Connected { tag: Tag, at: Address, made: io::Result<OwnedSocket> },
}

/// What the driver's threads are given.
enum Work {
    File(Effect),
    Connect { tag: Tag, at: Address },
}

struct Spawned {
    process: OwnedHandle,
    job: OwnedHandle,
    waiting: Option<process::Waiting>,
    serial: u64,
    out: Vec<u8>,
    err: Vec<u8>,
    /// Of its two outputs, how many have not ended.
    outputs: u8,
    exited: bool,
    /// The request writing its input, while it is in flight.
    input: Option<u64>,
    /// Why its output or input could not all be moved.
    failed: Option<io::Error>,
    finish: process::Finish,
}

struct Connection {
    socket: Arc<OwnedSocket>,
    recvs: VecDeque<(Tag, usize)>,
    sends: VecDeque<(Tag, Vec<u8>)>,
    /// One receive and one send in flight at a time, so their bytes keep their order.
    receiving: bool,
    sending: bool,
}

pub struct Iocp {
    start: Instant,
    root: Arc<Path>,
    port: Arc<Port>,
    back: Receiver<Back>,
    work: Option<Sender<Work>>,
    workers: Vec<JoinHandle<()>>,
    confine: Option<Arc<dyn Confine>>,
    readers: FxHashMap<FileName, Arc<OwnedHandle>>,
    appenders: FxHashMap<FileName, Arc<OwnedHandle>>,
    /// Every request in flight, by its slot's id.
    requests: FxHashMap<u64, Request>,
    next: u64,
    /// Completions made on the polling thread, in the order they happened.
    ready: Vec<Completion>,
    /// Due, then the order they were asked for.
    timers: BinaryHeap<Reverse<(Instant, u64, Tag)>>,
    timers_asked: u64,
    watches: FxHashMap<Tag, u64>,
    listens: FxHashMap<Tag, u64>,
    spawns: FxHashMap<Tag, Spawned>,
    serials: FxHashMap<u64, Tag>,
    next_serial: u64,
    conns: FxHashMap<Conn, Connection>,
    next_conn: u64,
    entries: Vec<OVERLAPPED_ENTRY>,
    in_flight: usize,
}

impl Iocp {
    /// A driver whose files are under `root`, with `threads` threads for the calls that block.
    pub fn new(root: &Path, threads: usize) -> io::Result<Iocp> {
        let root: Arc<Path> = root.into();
        let port = Arc::new(Port::new()?);
        let (sender, back) = mpsc::channel();
        let (work, queue) = mpsc::channel::<Work>();
        let queue = Arc::new(Mutex::new(queue));
        let workers = (0..threads.max(1))
            .map(|n| {
                let (root, queue, port, sender) = (root.clone(), queue.clone(), port.clone(), sender.clone());
                std::thread::Builder::new()
                    .name(format!("blaze-iocp-{n}"))
                    .spawn(move || work_on(&root, &queue, &port, &sender))
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Iocp {
            start: Instant::now(),
            root,
            port,
            back,
            work: Some(work),
            workers,
            confine: None,
            readers: FxHashMap::default(),
            appenders: FxHashMap::default(),
            requests: FxHashMap::default(),
            next: 0,
            ready: Vec::new(),
            timers: BinaryHeap::new(),
            timers_asked: 0,
            watches: FxHashMap::default(),
            listens: FxHashMap::default(),
            spawns: FxHashMap::default(),
            serials: FxHashMap::default(),
            next_serial: 0,
            conns: FxHashMap::default(),
            next_conn: 0,
            // SAFETY: all zeros is an entry of nothing, written over before it is read.
            entries: vec![unsafe { std::mem::zeroed() }; ENTRIES],
            in_flight: 0,
        })
    }

    /// The driver, starting each process whose sandbox is on as `confine` makes it.
    pub fn confined(mut self, confine: Arc<dyn Confine>) -> Iocp {
        self.confine = Some(confine);
        self
    }

    /// Where the TCP listen `listen` listens, for one asked of port 0.
    pub fn local_addr(&self, listen: Tag) -> Option<std::net::SocketAddr> {
        let id = self.listens.get(&listen)?;
        match &self.requests.get(id)?.op {
            Op::Accept(accepting) => accepting.listener.local_addr(),
            _ => None,
        }
    }

    fn path(&self, file: &FileName) -> PathBuf {
        self.root.join(file.as_str())
    }

    fn request(&mut self, tag: Tag, op: Op) -> Request {
        let id = self.next;
        self.next += 1;
        Request { tag, slot: Slot::new(id), op }
    }

    /// `request` asked for, kept until its completion; or, refused at once, ended now.
    fn put(&mut self, mut request: Request) {
        match request.ask() {
            Ok(()) => _ = self.requests.insert(request.slot.id, request),
            Err(error) => self.ended(request, Err(error)),
        }
    }

    fn fail(&mut self, tag: Tag, why: impl ToString) {
        self.ready.push(Completion { tag, done: Done::Failed(IoError(why.to_string())) });
    }

    fn worker(&self, work: Work) {
        let queue = self.work.as_ref().expect("the driver is running");
        queue.send(work).expect("the driver's threads are running");
    }

    /// The handles of `file` let go, so the next request opens the file now there: it is
    /// replaced, renamed or removed, or what is under it is.
    fn forget(&mut self, file: &FileName) {
        self.readers.retain(|name, _| !name.is_under(file));
        self.appenders.retain(|name, _| !name.is_under(file));
    }

    fn cached(&mut self, file: &FileName, access: files::Access) -> io::Result<Arc<OwnedHandle>> {
        let path = self.path(file);
        let cache = match access {
            files::Access::Append => &mut self.appenders,
            _ => &mut self.readers,
        };
        if let Some(handle) = cache.get(file) {
            return Ok(handle.clone());
        }
        let handle = Arc::new(files::open(&path, access)?);
        self.port.associate(handle.as_raw_handle())?;
        if cache.len() >= CACHED {
            cache.clear();
        }
        cache.insert(file.clone(), handle.clone());
        Ok(handle)
    }

    fn begin(&mut self, effect: Effect) -> io::Result<()> {
        match effect {
            Effect::ReadAll { file, tag } => self.read(&file, 0, None, tag),
            Effect::Read { file, at, len, tag } => self.read(&file, at, Some(len), tag),
            Effect::Append { file, bytes, tag } => {
                let handle = self.cached(&file, files::Access::Append)?;
                if bytes.is_empty() {
                    self.ready.push(Completion { tag, done: Done::Appended });
                    return Ok(());
                }
                let request = self.request(tag, Op::Write { handle, bytes, at: None, put: 0 });
                self.put(request);
                Ok(())
            }
            Effect::WriteAt { file, at, bytes, tag } => {
                let handle = Arc::new(files::open(&self.path(&file), files::Access::WriteAt)?);
                if bytes.is_empty() {
                    self.ready.push(Completion { tag, done: Done::Written });
                    return Ok(());
                }
                self.port.associate(handle.as_raw_handle())?;
                let request = self.request(tag, Op::Write { handle, bytes, at: Some(at), put: 0 });
                self.put(request);
                Ok(())
            }
            effect @ (Effect::Write { .. }
            | Effect::Rename { .. }
            | Effect::Clone { .. }
            | Effect::Link { .. }
            | Effect::Remove { .. }) => {
                let replaced = match &effect {
                    Effect::Write { file, .. } | Effect::Remove { file, .. } => vec![file.clone()],
                    Effect::Rename { from, to, .. } => vec![from.clone(), to.clone()],
                    Effect::Clone { to, .. } | Effect::Link { to, .. } => vec![to.clone()],
                    _ => Vec::new(),
                };
                for file in &replaced {
                    self.forget(file);
                }
                self.worker(Work::File(effect));
                Ok(())
            }
            effect @ (Effect::Sync { .. }
            | Effect::Truncate { .. }
            | Effect::Stat { .. }
            | Effect::List { .. }
            | Effect::Mode { .. }
            | Effect::Map { .. }) => {
                self.worker(Work::File(effect));
                Ok(())
            }
            Effect::Timer { after, tag } => {
                let now = Instant::now();
                // A timer past what a clock can say is due in some thirty years: never, here.
                let due = now.checked_add(after).unwrap_or_else(|| now + Duration::from_secs(1 << 30));
                self.timers.push(Reverse((due, self.timers_asked, tag)));
                self.timers_asked += 1;
                Ok(())
            }
            Effect::Watch { dir, tag } => {
                if self.watches.contains_key(&tag) {
                    return Err(io::Error::other(format!("something is watched as {tag:?} already")));
                }
                let watching = watch::Watching::new(&self.root, &dir)?;
                self.port.associate(watching.raw())?;
                let request = self.request(tag, Op::Watch(watching));
                self.watches.insert(tag, request.slot.id);
                self.put(request);
                Ok(())
            }
            Effect::Spawn { command, tag } => self.spawn(command, tag),
            Effect::Listen { at, tag } => self.listen(at, tag),
            Effect::Connect { at, tag } => self.connect(at, tag),
            Effect::Recv { conn, most, tag } => {
                self.connection(conn)?.recvs.push_back((tag, most));
                self.serve_recvs(conn);
                Ok(())
            }
            Effect::Send { conn, bytes, tag } => {
                self.connection(conn)?.sends.push_back((tag, bytes));
                self.serve_sends(conn);
                Ok(())
            }
            Effect::Hangup { conn, tag } => self.hangup(conn, tag),
            Effect::Kill { .. } | Effect::Unwatch { .. } | Effect::Unlisten { .. } => {
                unreachable!("taken where they are submitted")
            }
        }
    }

    fn read(&mut self, file: &FileName, at: u64, len: Option<usize>, tag: Tag) -> io::Result<()> {
        let handle = match self.cached(file, files::Access::Read) {
            Ok(handle) => handle,
            // A file that is not there reads as empty, read whole.
            Err(error) if error.kind() == io::ErrorKind::NotFound && len.is_none() => {
                self.ready.push(Completion { tag, done: Done::Read(Vec::new()) });
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let (bytes, to_end) = match len {
            Some(len) => (vec![0; len], false),
            // A byte past its length, so the read that fills it is the one that finds the end.
            None => (vec![0; files::size(&handle)? as usize + 1], true),
        };
        if bytes.is_empty() {
            self.ready.push(Completion { tag, done: Done::Read(Vec::new()) });
            return Ok(());
        }
        let request = self.request(tag, Op::Read { handle, bytes, at, filled: 0, to_end });
        self.put(request);
        Ok(())
    }

    fn spawn(&mut self, command: Box<Command>, tag: Tag) -> io::Result<()> {
        if self.spawns.contains_key(&tag) {
            return Err(io::Error::other(format!("a process runs as {tag:?} already")));
        }
        let serial = self.next_serial;
        self.next_serial += 1;
        let started = process::start(&self.root, command, self.confine.as_ref(), &self.port, serial)?;
        let process::Started { process, job, stdin, stdout, stderr, finish } = started;
        let port = &self.port;
        let followed = [Some(&stdout), Some(&stderr), stdin.as_ref()]
            .into_iter()
            .flatten()
            .try_for_each(|pipe| port.associate(pipe.as_raw_handle()))
            .and_then(|()| process::Waiting::new(&process, port, serial));
        let waiting = match followed {
            Ok(waiting) => waiting,
            Err(error) => {
                drop((stdin, stdout, stderr));
                abandon(&process, &job, finish);
                return Err(error);
            }
        };
        let bytes = stdin.as_ref().map(|_| finish.command.stdin.clone());
        self.serials.insert(serial, tag);
        self.spawns.insert(tag, Spawned {
            process,
            job,
            waiting: Some(waiting),
            serial,
            out: Vec::new(),
            err: Vec::new(),
            outputs: 2,
            exited: false,
            input: None,
            failed: None,
            finish,
        });
        for (err, pipe) in [(false, stdout), (true, stderr)] {
            let chunk = vec![0; CHUNK].into_boxed_slice();
            let request = self.request(tag, Op::Output { spawn: tag, err, pipe, chunk });
            self.put(request);
        }
        if let (Some(pipe), Some(bytes)) = (stdin, bytes) {
            let request = self.request(tag, Op::Input { spawn: tag, pipe, bytes, put: 0 });
            if let Some(spawned) = self.spawns.get_mut(&tag) {
                spawned.input = Some(request.slot.id);
            }
            self.put(request);
        }
        Ok(())
    }

    /// Ends the spawn `tag` once it has exited and its output has ended.
    fn finish_spawn(&mut self, tag: Tag) {
        match self.spawns.get(&tag) {
            Some(it) if it.exited && it.outputs == 0 => {}
            _ => return,
        }
        let spawned = self.spawns.remove(&tag).expect("looked up above");
        self.serials.remove(&spawned.serial);
        // What it did not read of its input is not written.
        if let Some(request) = spawned.input.and_then(|id| self.requests.get_mut(&id)) {
            request.cancel();
        }
        let Spawned { process, job, waiting, out, err, failed, finish, .. } = spawned;
        drop(waiting);
        let program = finish.command.program.display().to_string();
        let ended = finish.exited(&process, &job, out, err);
        // Closing the job kills whatever of it is left (`KILL_ON_JOB_CLOSE`).
        drop(job);
        let done = match (ended, failed) {
            (Ok(exit), None) => Done::Exited(Box::new(exit)),
            (Ok(_), Some(error)) | (Err(error), _) => Done::Failed(IoError(format!("{program}: {error}"))),
        };
        self.ready.push(Completion { tag, done });
    }

    fn listen(&mut self, at: Address, tag: Tag) -> io::Result<()> {
        if self.listens.contains_key(&tag) {
            return Err(io::Error::other(format!("something listens as {tag:?} already")));
        }
        let context =
            |error: io::Error| io::Error::new(error.kind(), format!("listening at {at:?}: {error}"));
        let listener = net::listen(&self.root, &at).map_err(context)?;
        self.port.associate(net::raw(&listener.socket) as HANDLE).map_err(context)?;
        let request = self.request(tag, Op::Accept(net::Accepting::new(listener)));
        self.listens.insert(tag, request.slot.id);
        self.put(request);
        Ok(())
    }

    fn connect(&mut self, at: Address, tag: Tag) -> io::Result<()> {
        if let Address::Unix(_) = at {
            self.worker(Work::Connect { tag, at });
            return Ok(());
        }
        let id = self.next;
        self.next += 1;
        let mut slot = Slot::new(id);
        let port = &self.port;
        let associate = |socket: &OwnedSocket| port.associate(net::raw(socket) as HANDLE);
        let connecting = net::Connecting::start(&self.root, &at, associate, slot.reset(0))
            .map_err(|error| io::Error::new(error.kind(), format!("connecting to {at:?}: {error}")))?;
        self.requests.insert(id, Request { tag, slot, op: Op::Connect { connecting, at } });
        Ok(())
    }

    fn adopt(&mut self, socket: OwnedSocket) -> Conn {
        let conn = Conn(self.next_conn);
        self.next_conn += 1;
        self.conns.insert(conn, Connection {
            socket: Arc::new(socket),
            recvs: VecDeque::new(),
            sends: VecDeque::new(),
            receiving: false,
            sending: false,
        });
        conn
    }

    /// A connect's end: `made`, its socket on the port, or why not.
    fn connected(&mut self, tag: Tag, at: &Address, made: io::Result<OwnedSocket>) {
        match made {
            Ok(socket) => {
                let conn = self.adopt(socket);
                self.ready.push(Completion { tag, done: Done::Connected { conn } });
            }
            Err(error) => self.fail(tag, format!("connecting to {at:?}: {error}")),
        }
    }

    fn connection(&mut self, conn: Conn) -> io::Result<&mut Connection> {
        self.conns.get_mut(&conn).ok_or_else(|| no_connection(conn))
    }

    fn serve_recvs(&mut self, conn: Conn) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        if connection.receiving {
            return;
        }
        let Some((tag, most)) = connection.recvs.pop_front() else { return };
        connection.receiving = true;
        let socket = connection.socket.clone();
        let bytes = vec![0; most.min(CHUNK)];
        let request = self.request(tag, Op::Recv { conn, socket, bytes, flags: Box::new(0) });
        self.put(request);
    }

    fn serve_sends(&mut self, conn: Conn) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        if connection.sending {
            return;
        }
        let Some((tag, bytes)) = connection.sends.pop_front() else { return };
        if bytes.is_empty() {
            self.ready.push(Completion { tag, done: Done::Sent });
            return self.serve_sends(conn);
        }
        connection.sending = true;
        let socket = connection.socket.clone();
        let request = self.request(tag, Op::Send { conn, socket, bytes, put: 0 });
        self.put(request);
    }

    fn hangup(&mut self, conn: Conn, tag: Tag) -> io::Result<()> {
        let connection = self.conns.remove(&conn).ok_or_else(|| no_connection(conn))?;
        let shut = net::hang_up(&connection.socket);
        // What is in flight on it ends now: a receive as empty, a send as failed (`ended`).
        // SAFETY: an open socket; no `OVERLAPPED` names every request on it.
        unsafe { CancelIoEx(net::raw(&connection.socket) as HANDLE, std::ptr::null()) };
        for (recv, _) in connection.recvs {
            self.ready.push(Completion { tag: recv, done: Done::Read(Vec::new()) });
        }
        for (send, _) in connection.sends {
            self.fail(send, format!("{conn:?} hung up before all of it was sent"));
        }
        shut?;
        self.ready.push(Completion { tag, done: Done::HungUp });
        Ok(())
    }

    /// What a request's step came to: its next step asked for, or its end.
    fn ended(&mut self, mut request: Request, result: io::Result<u32>) {
        let tag = request.tag;
        if let Op::Connect { .. } = request.op {
            let Op::Connect { connecting, at } = request.op else { unreachable!("matched above") };
            let made = result.and_then(|_| connecting.take());
            return self.connected(tag, &at, made);
        }
        let code = result.as_ref().err().and_then(io::Error::raw_os_error).map(|it| it as u32);
        match &mut request.op {
            Op::Read { bytes, filled, to_end, .. } => {
                let got = match result {
                    Ok(got) => got as usize,
                    Err(_) if code == Some(ERROR_HANDLE_EOF) => 0,
                    Err(error) => return self.fail(tag, error),
                };
                *filled += got;
                if got > 0 && *to_end && *filled == bytes.len() {
                    bytes.resize(bytes.len() * 2, 0);
                }
                if got > 0 && *filled < bytes.len() {
                    return self.put(request);
                }
                bytes.truncate(*filled);
                let bytes = std::mem::take(bytes);
                self.ready.push(Completion { tag, done: Done::Read(bytes) });
            }
            Op::Write { bytes, at, put, .. } => {
                let done = match result {
                    Ok(0) if *put < bytes.len() => Done::Failed(IoError("the file took no more bytes".into())),
                    Ok(got) => {
                        *put += got as usize;
                        if *put < bytes.len() {
                            return self.put(request);
                        }
                        match at {
                            Some(_) => Done::Written,
                            None => Done::Appended,
                        }
                    }
                    Err(error) => Done::Failed(IoError(error.to_string())),
                };
                self.ready.push(Completion { tag, done });
            }
            Op::Watch(watching) => {
                let stopping = watching.stopping;
                match result {
                    Ok(got) => {
                        // A buffer of nothing while stopping is the cancel, not a loss.
                        if !(stopping && got == 0) {
                            let changed = watching.take(got as usize);
                            self.ready.push(Completion { tag, done: changed });
                        }
                        if !stopping {
                            return self.put(request);
                        }
                    }
                    Err(_) if code == Some(ERROR_NOTIFY_ENUM_DIR) && !stopping => {
                        self.ready.push(Completion { tag, done: Done::Changed { files: Vec::new(), lost: true } });
                        return self.put(request);
                    }
                    Err(_) if stopping => {}
                    Err(error) => {
                        self.watches.remove(&tag);
                        return self.fail(tag, format!("watching: {error}"));
                    }
                }
                self.watches.remove(&tag);
                self.ready.push(Completion { tag, done: Done::Unwatched });
            }
            Op::Output { spawn, err, chunk, .. } => {
                let (spawn, err) = (*spawn, *err);
                let Some(spawned) = self.spawns.get_mut(&spawn) else { return };
                match result {
                    Ok(got) => {
                        let into = if err { &mut spawned.err } else { &mut spawned.out };
                        into.extend_from_slice(&chunk[..got as usize]);
                        return self.put(request);
                    }
                    // The last writer closed it: the end of the output.
                    Err(_) if code == Some(ERROR_BROKEN_PIPE) => {}
                    Err(error) => {
                        process::terminate(&spawned.job, process::KILLED);
                        spawned.failed.get_or_insert(error);
                    }
                }
                spawned.outputs -= 1;
                drop(request);
                self.finish_spawn(spawn);
            }
            Op::Input { spawn, bytes, put, .. } => {
                let Some(spawned) = self.spawns.get_mut(spawn) else { return };
                match result {
                    Ok(got) => {
                        *put += got as usize;
                        if got > 0 && *put < bytes.len() {
                            return self.put(request);
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
            }
            Op::Accept(accepting) => {
                if accepting.stopping {
                    // A connection accepted as it was asked to stop closes with it.
                    self.listens.remove(&tag);
                    drop(request);
                    self.ready.push(Completion { tag, done: Done::Unlistened });
                    return;
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
                                let conn = self.adopt(socket);
                                self.ready.push(Completion { tag, done: Done::Accepted { conn } });
                            }
                            Err(error) => {
                                self.listens.remove(&tag);
                                return self.fail(tag, format!("accepting a connection: {error}"));
                            }
                        }
                    }
                    // The peer went before it was accepted.
                    Err(_) if matches!(code, Some(ERROR_NETNAME_DELETED | ERROR_CONNECTION_ABORTED))
                        || code == Some(WSAECONNRESET as u32) => {}
                    Err(error) => {
                        self.listens.remove(&tag);
                        return self.fail(tag, format!("accepting a connection: {error}"));
                    }
                }
                self.put(request);
            }
            Op::Connect { .. } => unreachable!("ended above"),
            Op::Recv { conn, bytes, .. } => {
                let conn = *conn;
                let done = match result {
                    Ok(got) => {
                        bytes.truncate(got as usize);
                        Done::Read(std::mem::take(bytes))
                    }
                    // Hung up, here or by the peer: no more bytes.
                    Err(_)
                        if matches!(
                            code,
                            Some(ERROR_OPERATION_ABORTED | ERROR_NETNAME_DELETED | ERROR_CONNECTION_ABORTED)
                        ) || code == Some(WSAECONNRESET as u32) =>
                    {
                        Done::Read(Vec::new())
                    }
                    Err(error) => Done::Failed(IoError(error.to_string())),
                };
                drop(request);
                self.ready.push(Completion { tag, done });
                if let Some(connection) = self.conns.get_mut(&conn) {
                    connection.receiving = false;
                }
                self.serve_recvs(conn);
            }
            Op::Send { conn, bytes, put, .. } => {
                let conn = *conn;
                let done = match result {
                    Ok(got) => {
                        *put += got as usize;
                        if got > 0 && *put < bytes.len() && self.conns.contains_key(&conn) {
                            return self.put(request);
                        }
                        match *put == bytes.len() {
                            true => Done::Sent,
                            false => Done::Failed(IoError(format!("{conn:?} took no more bytes"))),
                        }
                    }
                    Err(_) if code == Some(ERROR_OPERATION_ABORTED) => {
                        Done::Failed(IoError(format!("{conn:?} hung up before all of it was sent")))
                    }
                    Err(error) => Done::Failed(IoError(error.to_string())),
                };
                drop(request);
                self.ready.push(Completion { tag, done });
                if let Some(connection) = self.conns.get_mut(&conn) {
                    connection.sending = false;
                }
                self.serve_sends(conn);
            }
        }
    }

    /// Takes what the port has, waiting up to `wait` for the first, and acts on it.
    fn wait(&mut self, wait: Duration) {
        let mut entries = std::mem::take(&mut self.entries);
        let got = self.port.take(&mut entries, wait).expect("waiting on the completion port");
        for entry in &entries[..got] {
            match entry.lpCompletionKey {
                KEY_IO => {
                    // SAFETY: every packet of this key is a request's, whose slot is in
                    // `requests` until it is taken here.
                    let id = unsafe { (*entry.lpOverlapped.cast::<Slot>()).id };
                    let request = self.requests.remove(&id).expect("a request in flight");
                    let result = request.slot.result().map_err(|code| io::Error::from_raw_os_error(code as i32));
                    self.ended(request, result);
                }
                KEY_WAKE => {}
                KEY_EXIT => {
                    let serial = entry.lpOverlapped as usize as u64;
                    let Some(&tag) = self.serials.get(&serial) else { continue };
                    if let Some(spawned) = self.spawns.get_mut(&tag) {
                        spawned.exited = true;
                    }
                    self.finish_spawn(tag);
                }
                key if key >= KEY_JOB => {
                    let serial = (key - KEY_JOB) as u64;
                    let Some(tag) = self.serials.get(&serial) else { continue };
                    let Some(spawned) = self.spawns.get(tag) else { continue };
                    // Past its memory: the commit the job is refused is the backstop, and the
                    // kill makes it the end.
                    if entry.dwNumberOfBytesTransferred == JOB_OBJECT_MSG_JOB_MEMORY_LIMIT {
                        process::terminate(&spawned.job, process::OUT_OF_MEMORY);
                    }
                }
                _ => {}
            }
        }
        self.entries = entries;
    }

    /// Every timer due fired, in the order they were due, and what the threads sent taken.
    fn gather(&mut self, out: &mut Vec<Completion>) {
        let now = Instant::now();
        while let Some(&Reverse((due, _, tag))) = self.timers.peek() {
            if due > now {
                break;
            }
            self.timers.pop();
            self.ready.push(Completion { tag, done: Done::Fired });
        }
        while let Ok(back) = self.back.try_recv() {
            match back {
                Back::Done(completion) => self.ready.push(completion),
                Back::Connected { tag, at, made } => {
                    let made = made.and_then(|socket| {
                        self.port.associate(net::raw(&socket) as HANDLE)?;
                        Ok(socket)
                    });
                    self.connected(tag, &at, made);
                }
            }
        }
        out.append(&mut self.ready);
    }
}

impl Drop for Iocp {
    fn drop(&mut self) {
        for spawned in self.spawns.values() {
            process::terminate(&spawned.job, process::KILLED);
        }
        for request in self.requests.values_mut() {
            request.cancel();
        }
        // Each request is let go only once the port gives it back: until then the kernel may
        // still write to what it lent.
        let deadline = Instant::now() + GIVE_BACK;
        let mut entries = std::mem::take(&mut self.entries);
        while !self.requests.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let Ok(got) = self.port.take(&mut entries, left) else { break };
            for entry in &entries[..got] {
                if entry.lpCompletionKey == KEY_IO {
                    // SAFETY: as in `wait`.
                    let id = unsafe { (*entry.lpOverlapped.cast::<Slot>()).id };
                    self.requests.remove(&id);
                }
            }
        }
        // What never came back stays lent: leaked, so nothing the kernel may write is freed.
        for (_, request) in self.requests.drain() {
            std::mem::forget(request);
        }
        for (_, spawned) in self.spawns.drain() {
            let Spawned { process, job, waiting, finish, .. } = spawned;
            drop(waiting);
            abandon(&process, &job, finish);
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
                    if let Some(spawned) = self.spawns.get(&tag) {
                        process::terminate(&spawned.job, process::KILLED);
                    }
                }
                // The end of the watch or listen, in flight since it was asked for, comes with
                // its request's completion: after every change it heard.
                Effect::Unwatch { tag } => match self.watches.get(&tag).and_then(|id| self.requests.get_mut(id)) {
                    Some(request) => {
                        if let Op::Watch(watching) = &mut request.op {
                            watching.stopping = true;
                        }
                        request.cancel();
                    }
                    None => {
                        self.in_flight += 1;
                        self.fail(tag, format!("nothing is watched as {tag:?}"));
                    }
                },
                Effect::Unlisten { tag } => match self.listens.get(&tag).and_then(|id| self.requests.get_mut(id)) {
                    Some(request) => {
                        if let Op::Accept(accepting) = &mut request.op {
                            accepting.stopping = true;
                        }
                        request.cancel();
                    }
                    None => {
                        self.in_flight += 1;
                        self.fail(tag, format!("nothing listens as {tag:?}"));
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
            if let Some(Reverse((due, ..))) = self.timers.peek() {
                left = left.min(due.saturating_duration_since(now));
            }
            self.wait(left);
            self.gather(out);
            if out.len() > before || deadline.is_some_and(|it| Instant::now() >= it) {
                break;
            }
        }
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

fn work_on(root: &Path, queue: &Mutex<Receiver<Work>>, port: &Port, back: &Sender<Back>) {
    loop {
        let next = queue.lock().map(|it| it.recv());
        let Ok(Ok(work)) = next else { return };
        let sent = match work {
            Work::File(effect) => {
                let tag = effect.tag();
                let done = file(root, effect).unwrap_or_else(|error| Done::Failed(IoError(error.to_string())));
                back.send(Back::Done(Completion { tag, done }))
            }
            Work::Connect { tag, at } => {
                let made = net::connect_blocking(root, &at);
                back.send(Back::Connected { tag, at, made })
            }
        };
        if sent.is_err() || port.post(KEY_WAKE, 0, 0).is_err() {
            return;
        }
    }
}

/// A file effect that blocks, as Windows does it.
fn file(root: &Path, effect: Effect) -> io::Result<Done> {
    let at = |file: &FileName| root.join(file.as_str());
    match effect {
        Effect::Write { file, bytes, .. } => files::write_atomically(&at(&file), &bytes).map(|()| Done::Written),
        Effect::Rename { from, to, .. } => files::rename(&at(&from), &at(&to)).map(|()| Done::Renamed),
        Effect::Mode { file, mode, .. } => files::set_mode(&at(&file), mode).map(|()| Done::Moded),
        Effect::Clone { from, to, .. } => files::clone(&at(&from), &at(&to)).map(|()| Done::Cloned),
        Effect::Map { file, .. } => files::map(&at(&file)).map(Done::Mapped),
        effect => crate::threads::run(root, effect),
    }
}

/// A process the driver no longer follows: killed, waited for, and what was made for it
/// removed.
fn abandon(process: &OwnedHandle, job: &OwnedHandle, finish: process::Finish) {
    process::terminate(job, process::KILLED);
    // SAFETY: an open process handle; a kill of its job ends it promptly.
    unsafe { WaitForSingleObject(process.as_raw_handle(), GIVE_BACK.as_millis() as u32) };
    _ = finish.exited(process, job, Vec::new(), Vec::new());
}

fn no_connection(conn: Conn) -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, format!("no connection {conn:?}"))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString};

    use super::*;
    use crate::{Confined, Exit, Sandbox, Traced, run_until_idle, threads::Threads};

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

    fn run(driver: &mut dyn Driver, effects: Vec<Effect>) -> BTreeMap<u64, Done> {
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
        let script = |driver: &mut dyn Driver| {
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
        };
        let (ours, theirs) = (dir("files"), dir("files-threads"));
        let mut iocp = Iocp::new(&ours, 2).unwrap();
        let ran = script(&mut iocp);
        let threads = script(&mut Threads::new(&theirs, 2));
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
}
