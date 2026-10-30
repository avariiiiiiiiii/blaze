//! A [`Driver`] for macOS: one kqueue the polling thread waits on, for processes, sockets and
//! the end of work done elsewhere; timers are a [`Wheel`] whose next deadline ends the wait,
//! with no registration in the kernel. File calls block, so they run on threads of the
//! driver's own; watches and journals are FSEvents streams on dispatch queues of their own.
//! Both hand their completions back through a channel and wake the kqueue with an
//! `EVFILT_USER` trigger.
//!
//! Every registration carries an id of the driver's in `udata`, so an event of a descriptor
//! closed and reused within one batch is told apart from the new one's and dropped. Changes are
//! made with the next wait, all in one call, their failures coming back as events.

use std::{
    collections::VecDeque,
    io,
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use rustc_hash::FxHashMap;

use crate::{
    Address, Command, Completion, Confine, Conn, Done, Driver, Effect, IoError, Tag, fs, fsevents,
    net, process,
    wheel::{self, Wheel},
};

/// The id of the trigger that says the channel has completions.
const TRIGGER: u64 = 0;
/// Events taken off the kqueue at once, besides the failures of the changes made with them.
const EVENTS: usize = 64;

/// `struct kevent` as the system lays it out, with `udata` the driver's id, so the lists of
/// changes and events are plain data of the driver's own.
#[repr(C)]
#[derive(Clone, Copy)]
struct Kevent {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
    udata: usize,
}

const _: () = assert!(
    size_of::<Kevent>() == size_of::<libc::kevent>()
        && align_of::<Kevent>() == align_of::<libc::kevent>()
);

/// The kqueue and the channel, shared with the file threads and the watches' queues.
struct Notify {
    kq: OwnedFd,
    done: Sender<Completion>,
}

impl Notify {
    fn change(
        &self,
        ident: usize,
        filter: i16,
        flags: u16,
        fflags: u32,
        data: isize,
        id: u64,
    ) -> io::Result<()> {
        let event = Kevent { ident, filter, flags, fflags, data, udata: id as usize };
        loop {
            // SAFETY: one change, laid out as a `kevent`, to read and no events to write.
            let changed = unsafe {
                libc::kevent(
                    self.kq.as_raw_fd(),
                    (&raw const event).cast(),
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if changed >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Hands `completion` to the polling thread and wakes it; false once the driver is gone.
    fn send(&self, completion: Completion) -> bool {
        if self.done.send(completion).is_err() {
            return false;
        }
        _ = self.change(TRIGGER as usize, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0, TRIGGER);
        true
    }
}

/// What an event is about, by the id in its `udata`.
#[derive(Clone, Copy)]
enum Owner {
    Exit(Tag),
    Output { tag: Tag, err: bool },
    Input(Tag),
    Listen(Tag),
    Connecting(Tag),
    Read(Conn),
    Write(Conn),
}

struct Spawned {
    pid: libc::pid_t,
    /// Open until its input is all written.
    stdin: Option<OwnedFd>,
    written: usize,
    /// Open until each ends.
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
    out: Vec<u8>,
    err: Vec<u8>,
    exited: bool,
    /// Why its output or input could not all be moved.
    failed: Option<io::Error>,
    finish: process::Finish,
    ids: Vec<u64>,
}

struct Listening {
    listener: net::Listener,
    id: u64,
}

struct Connecting {
    fd: OwnedFd,
    at: Address,
    unix: bool,
}

struct Connection {
    socket: net::Socket,
    recvs: VecDeque<(Tag, usize)>,
    /// Each send, and how much of it is sent.
    sends: VecDeque<(Tag, Vec<u8>, usize)>,
    read: u64,
    write: u64,
}

pub struct Kqueue {
    start: Instant,
    root: Arc<Path>,
    notify: Arc<Notify>,
    done: Receiver<Completion>,
    work: Option<crossbeam_channel::Sender<Effect>>,
    workers: Vec<JoinHandle<()>>,
    confine: Option<Arc<dyn Confine>>,
    /// Where a process's output and a connection's bytes are read into.
    buffer: Vec<u8>,
    /// Made with the next wait.
    changes: Vec<Kevent>,
    events: Vec<Kevent>,
    /// Completions made on the polling thread, in the order they happened.
    ready: Vec<Completion>,
    next_id: u64,
    owners: FxHashMap<u64, Owner>,
    /// By ticks since `start`; the wait ends at the next one due.
    timers: Wheel<Tag>,
    watches: FxHashMap<Tag, fsevents::Stream>,
    spawns: FxHashMap<Tag, Spawned>,
    listens: FxHashMap<Tag, Listening>,
    connecting: FxHashMap<Tag, Connecting>,
    conns: FxHashMap<Conn, Connection>,
    next_conn: u64,
    in_flight: usize,
}

impl Kqueue {
    /// A driver whose files are under `root`, with `threads` threads for the file calls.
    pub fn new(root: impl Into<PathBuf>, threads: usize) -> io::Result<Kqueue> {
        let root: Arc<Path> = root.into().into();
        // SAFETY: no pointers; a descriptor of its own or -1.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call returned a descriptor of its own.
        let kq = unsafe { OwnedFd::from_raw_fd(kq) };
        net::set_cloexec(kq.as_raw_fd())?;
        let (finished, done) = mpsc::channel();
        let notify = Arc::new(Notify { kq, done: finished });
        notify.change(
            TRIGGER as usize,
            libc::EVFILT_USER,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
            0,
            TRIGGER,
        )?;
        let (work, queue) = crossbeam_channel::unbounded::<Effect>();
        let workers = (0..threads.max(1))
            .map(|n| {
                let (root, queue, notify) = (root.clone(), queue.clone(), notify.clone());
                std::thread::Builder::new()
                    .name(format!("blaze-kqueue-{n}"))
                    .spawn(move || work_on(&root, &queue, &notify))
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Kqueue {
            start: Instant::now(),
            root,
            notify,
            done,
            work: Some(work),
            workers,
            confine: None,
            buffer: vec![0; 64 << 10],
            changes: Vec::new(),
            events: Vec::new(),
            ready: Vec::new(),
            next_id: TRIGGER + 1,
            owners: FxHashMap::default(),
            timers: Wheel::new(),
            watches: FxHashMap::default(),
            spawns: FxHashMap::default(),
            listens: FxHashMap::default(),
            connecting: FxHashMap::default(),
            conns: FxHashMap::default(),
            next_conn: 0,
            in_flight: 0,
        })
    }

    /// The driver, starting each process whose sandbox is on as `confine` makes it.
    pub fn confined(mut self, confine: Arc<dyn Confine>) -> Kqueue {
        self.confine = Some(confine);
        self
    }

    fn own(&mut self, owner: Owner) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.owners.insert(id, owner);
        id
    }

    /// A change made with the next wait; a failure of it comes back as an event of `id`'s with
    /// `EV_ERROR`.
    fn change(&mut self, ident: usize, filter: i16, flags: u16, fflags: u32, data: isize, id: u64) {
        self.changes.push(Kevent { ident, filter, flags, fflags, data, udata: id as usize });
    }

    /// `ident` of `filter` registered as `owner`'s, with the next wait.
    fn register(&mut self, owner: Owner, ident: usize, filter: i16, flags: u16, fflags: u32) -> u64 {
        let id = self.own(owner);
        self.change(ident, filter, flags, fflags, 0, id);
        id
    }

    /// `id` ends: no event of it is acted on, and a change of it not yet made is dropped, so it
    /// cannot land on a descriptor closed and reused since.
    fn disown(&mut self, id: u64) {
        self.owners.remove(&id);
        self.changes.retain(|it| it.udata != id as usize);
    }

    fn begin(&mut self, effect: Effect) -> io::Result<()> {
        match effect {
            effect @ (Effect::ReadAll { .. }
            | Effect::Read { .. }
            | Effect::Append { .. }
            | Effect::Sync { .. }
            | Effect::Truncate { .. }
            | Effect::Write { .. }
            | Effect::WriteAt { .. }
            | Effect::Rename { .. }
            | Effect::Mode { .. }
            | Effect::Clone { .. }
            | Effect::Link { .. }
            | Effect::Stat { .. }
            | Effect::List { .. }
            | Effect::Remove { .. }
            | Effect::Stamp { .. }
            | Effect::Entries { .. }
            | Effect::Map { .. }) => {
                let work = self.work.as_ref().expect("the driver is running");
                work.send(effect).expect("the IO threads are running");
                Ok(())
            }
            Effect::Timer { after, tag } => {
                let due = wheel::ticks(self.start.elapsed()).saturating_add(wheel::ticks(after));
                self.timers.insert(due, tag);
                Ok(())
            }
            Effect::Watch { dir, tag } => {
                let notify = self.notify.clone();
                let send = Box::new(move |completion| _ = notify.send(completion));
                let stream = fsevents::Stream::start(&self.root, &dir, tag, send)?;
                if let Some(old) = self.watches.insert(tag, stream) {
                    old.stop();
                }
                Ok(())
            }
            Effect::Journal { dir, since, tag } => {
                let notify = self.notify.clone();
                let send = Box::new(move |completion| _ = notify.send(completion));
                let stream = fsevents::Stream::journal(&self.root, &dir, since, tag, send)?;
                if let Some(old) = self.watches.insert(tag, stream) {
                    old.stop();
                }
                Ok(())
            }
            // Told through the channel by the stream, after the changes it heard first.
            Effect::Flush { watch, tag } => match self.watches.get(&watch) {
                Some(stream) => stream.flush(tag).map_err(|error| {
                    io::Error::new(error.kind(), format!("flushing the watch: {error}"))
                }),
                None => Err(io::Error::other(format!("nothing is watched as {watch:?}"))),
            },
            Effect::Spawn { command, tag } => self.spawn(command, tag),
            Effect::Listen { at, tag } => self.listen(&at, tag),
            Effect::Connect { at, tag } => self.connect(at, tag),
            Effect::Recv { conn, most, tag } => {
                self.connection(conn)?.recvs.push_back((tag, most));
                self.serve_recvs(conn);
                Ok(())
            }
            Effect::Send { conn, bytes, tag } => {
                self.connection(conn)?.sends.push_back((tag, bytes, 0));
                self.serve_sends(conn);
                Ok(())
            }
            Effect::Hangup { conn, tag } => self.hangup(conn, tag),
            Effect::Kill { .. } | Effect::Unwatch { .. } | Effect::Unlisten { .. } => {
                unreachable!("taken where they are submitted")
            }
        }
    }

    fn connection(&mut self, conn: Conn) -> io::Result<&mut Connection> {
        self.conns.get_mut(&conn).ok_or_else(|| no_connection(conn))
    }

    fn spawn(&mut self, command: Box<Command>, tag: Tag) -> io::Result<()> {
        let process::Started { pid, stdin, stdout, stderr, finish } =
            process::start(&self.root, command, self.confine.as_ref())?;
        let mut spawned = Spawned {
            pid,
            stdin,
            written: 0,
            stdout: Some(stdout),
            stderr: Some(stderr),
            out: Vec::new(),
            err: Vec::new(),
            exited: false,
            failed: None,
            finish,
            ids: Vec::new(),
        };
        match self.follow(tag, &mut spawned) {
            Ok(()) => {
                self.spawns.insert(tag, spawned);
                Ok(())
            }
            Err(error) => {
                for &id in &spawned.ids {
                    self.disown(id);
                }
                let program = spawned.finish.command.program.display().to_string();
                abandon(spawned);
                Err(io::Error::new(error.kind(), format!("following {program}: {error}")))
            }
        }
    }

    /// The process's pipes and its end registered on the kqueue.
    fn follow(&mut self, tag: Tag, spawned: &mut Spawned) -> io::Result<()> {
        for (fd, err) in [(&spawned.stdout, false), (&spawned.stderr, true)] {
            let fd = fd.as_ref().expect("a pipe just made").as_raw_fd();
            set_nonblocking(fd)?;
            let owner = Owner::Output { tag, err };
            let id = self.register(owner, fd as usize, libc::EVFILT_READ, libc::EV_ADD, 0);
            spawned.ids.push(id);
        }
        if let Some(fd) = &spawned.stdin {
            let fd = fd.as_raw_fd();
            set_nonblocking(fd)?;
            let id =
                self.register(Owner::Input(tag), fd as usize, libc::EVFILT_WRITE, libc::EV_ADD, 0);
            spawned.ids.push(id);
        }
        let (pid, flags) = (spawned.pid as usize, libc::EV_ADD | libc::EV_ONESHOT);
        let id = self.register(Owner::Exit(tag), pid, libc::EVFILT_PROC, flags, libc::NOTE_EXIT);
        spawned.ids.push(id);
        Ok(())
    }

    fn read_output(&mut self, tag: Tag, err: bool) {
        let Some(spawned) = self.spawns.get_mut(&tag) else { return };
        let (pipe, into) = match err {
            true => (&mut spawned.stderr, &mut spawned.err),
            false => (&mut spawned.stdout, &mut spawned.out),
        };
        let Some(fd) = pipe.as_ref().map(|it| it.as_raw_fd()) else { return };
        loop {
            // SAFETY: an open descriptor and a buffer of the length given.
            let n = unsafe { libc::read(fd, self.buffer.as_mut_ptr().cast(), self.buffer.len()) };
            match n {
                1.. => into.extend_from_slice(&self.buffer[..n as usize]),
                0 => {
                    *pipe = None;
                    break;
                }
                _ => {
                    let error = io::Error::last_os_error();
                    match error.kind() {
                        io::ErrorKind::Interrupted => continue,
                        io::ErrorKind::WouldBlock => break,
                        _ => {
                            spawned.failed.get_or_insert(error);
                            *pipe = None;
                            break;
                        }
                    }
                }
            }
        }
        self.finish_spawn(tag);
    }

    fn write_input(&mut self, tag: Tag) {
        let Some(spawned) = self.spawns.get_mut(&tag) else { return };
        let Some(fd) = spawned.stdin.as_ref().map(|it| it.as_raw_fd()) else { return };
        let input = &spawned.finish.command.stdin;
        while spawned.written < input.len() {
            let rest = &input[spawned.written..];
            // SAFETY: an open descriptor and `rest.len()` bytes to read.
            let n = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
            match n {
                1.. => spawned.written += n as usize,
                0 => {
                    spawned.failed.get_or_insert(io::ErrorKind::WriteZero.into());
                    break;
                }
                _ => {
                    let error = io::Error::last_os_error();
                    match error.kind() {
                        io::ErrorKind::Interrupted => {}
                        io::ErrorKind::WouldBlock => return,
                        // It stopped reading: what it did not read it was not given, as with
                        // any pipe.
                        io::ErrorKind::BrokenPipe => break,
                        _ => {
                            spawned.failed.get_or_insert(error);
                            break;
                        }
                    }
                }
            }
        }
        // Closed: the end of its input.
        spawned.stdin = None;
    }

    /// Ends the spawn `tag` once it has exited and its output has ended.
    fn finish_spawn(&mut self, tag: Tag) {
        match self.spawns.get(&tag) {
            Some(it) if it.exited && it.stdout.is_none() && it.stderr.is_none() => {}
            _ => return,
        }
        let spawned = self.spawns.remove(&tag).expect("looked up above");
        for &id in &spawned.ids {
            self.disown(id);
        }
        let Spawned { pid, out, err, failed, finish, .. } = spawned;
        let program = finish.command.program.display().to_string();
        let ended = finish.exited(pid, out, err);
        let done = match (ended, failed) {
            (Ok(exit), None) => Done::Exited(Box::new(exit)),
            (Ok(_), Some(error)) | (Err(error), _) => Done::Failed(IoError(format!("{program}: {error}"))),
        };
        self.ready.push(Completion { tag, done });
    }

    fn listen(&mut self, at: &Address, tag: Tag) -> io::Result<()> {
        let context =
            |error: io::Error| io::Error::new(error.kind(), format!("listening at {at:?}: {error}"));
        let listener = net::listen(&self.root, at).map_err(context)?;
        listener.set_nonblocking(true).map_err(context)?;
        let fd = listener.fd() as usize;
        let id = self.register(Owner::Listen(tag), fd, libc::EVFILT_READ, libc::EV_ADD, 0);
        self.listens.insert(tag, Listening { listener, id });
        Ok(())
    }

    fn accept(&mut self, tag: Tag) {
        loop {
            let Some(listening) = self.listens.get(&tag) else { return };
            let accepted = listening.listener.accept().and_then(|socket| {
                socket.set_nonblocking(true)?;
                no_sigpipe(socket.fd())?;
                Ok(socket)
            });
            match accepted {
                Ok(socket) => {
                    let conn = self.adopt(socket);
                    self.ready.push(Completion { tag, done: Done::Accepted { conn } });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                    ) => {}
                Err(error) => {
                    // A failure ends the listen.
                    let listening = self.listens.remove(&tag).expect("looked up above");
                    self.disown(listening.id);
                    let done = Done::Failed(IoError(format!("accepting a connection: {error}")));
                    self.ready.push(Completion { tag, done });
                    return;
                }
            }
        }
    }

    fn adopt(&mut self, socket: net::Socket) -> Conn {
        let conn = Conn(self.next_conn);
        self.next_conn += 1;
        let read = self.own(Owner::Read(conn));
        let write = self.own(Owner::Write(conn));
        let connection =
            Connection { socket, recvs: VecDeque::new(), sends: VecDeque::new(), read, write };
        self.conns.insert(conn, connection);
        conn
    }

    fn connect(&mut self, at: Address, tag: Tag) -> io::Result<()> {
        let context =
            |error: io::Error| io::Error::new(error.kind(), format!("connecting to {at:?}: {error}"));
        let address = net::sockaddr(&self.root, &at).map_err(context)?;
        let fd = net::stream_socket(address.domain).map_err(context)?;
        set_nonblocking(fd.as_raw_fd()).map_err(context)?;
        no_sigpipe(fd.as_raw_fd()).map_err(context)?;
        let unix = address.domain == libc::AF_UNIX;
        // SAFETY: an address of `len` bytes, of the socket's domain.
        let connected = unsafe {
            libc::connect(fd.as_raw_fd(), (&raw const address.storage).cast(), address.len)
        };
        if connected == 0 {
            return self.connected(tag, fd, unix).map_err(context);
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::EINPROGRESS | libc::EINTR)) {
            return Err(context(error));
        }
        let ident = fd.as_raw_fd() as usize;
        let flags = libc::EV_ADD | libc::EV_ONESHOT;
        self.register(Owner::Connecting(tag), ident, libc::EVFILT_WRITE, flags, 0);
        self.connecting.insert(tag, Connecting { fd, at, unix });
        Ok(())
    }

    /// A connect in flight has ended, made or not.
    fn ended_connecting(&mut self, tag: Tag) {
        let Some(Connecting { fd, at, unix }) = self.connecting.remove(&tag) else { return };
        let made = socket_error(fd.as_raw_fd()).and_then(|()| self.connected(tag, fd, unix));
        if let Err(error) = made {
            let done = Done::Failed(IoError(format!("connecting to {at:?}: {error}")));
            self.ready.push(Completion { tag, done });
        }
    }

    fn connected(&mut self, tag: Tag, fd: OwnedFd, unix: bool) -> io::Result<()> {
        let socket = net::Socket::from_fd(fd, unix);
        if let net::Socket::Tcp(stream) = &socket {
            stream.set_nodelay(true)?;
        }
        let conn = self.adopt(socket);
        self.ready.push(Completion { tag, done: Done::Connected { conn } });
        Ok(())
    }

    fn serve_recvs(&mut self, conn: Conn) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        while let Some(&(tag, most)) = connection.recvs.front() {
            // At most a buffer's worth, which a receive may be: nothing is allocated for a
            // receive that would wait.
            let most = most.min(self.buffer.len());
            let done = match connection.socket.read(&mut self.buffer[..most]) {
                Ok(n) => Done::Read(self.buffer[..n].to_vec()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.changes.push(Kevent {
                        ident: connection.socket.fd() as usize,
                        filter: libc::EVFILT_READ,
                        flags: libc::EV_ADD | libc::EV_ONESHOT,
                        fflags: 0,
                        data: 0,
                        udata: connection.read as usize,
                    });
                    return;
                }
                Err(error) => Done::Failed(IoError(error.to_string())),
            };
            connection.recvs.pop_front();
            self.ready.push(Completion { tag, done });
        }
    }

    fn serve_sends(&mut self, conn: Conn) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        while let Some((tag, bytes, put)) = connection.sends.front_mut() {
            let tag = *tag;
            let done = match *put == bytes.len() {
                true => Done::Sent,
                false => match connection.socket.write(&bytes[*put..]) {
                    Ok(0) => Done::Failed(IoError("the connection took no more bytes".into())),
                    Ok(n) => {
                        *put += n;
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        self.changes.push(Kevent {
                            ident: connection.socket.fd() as usize,
                            filter: libc::EVFILT_WRITE,
                            flags: libc::EV_ADD | libc::EV_ONESHOT,
                            fflags: 0,
                            data: 0,
                            udata: connection.write as usize,
                        });
                        return;
                    }
                    Err(error) => Done::Failed(IoError(error.to_string())),
                },
            };
            connection.sends.pop_front();
            self.ready.push(Completion { tag, done });
        }
    }

    fn hangup(&mut self, conn: Conn, tag: Tag) -> io::Result<()> {
        let connection = self.conns.remove(&conn).ok_or_else(|| no_connection(conn))?;
        self.disown(connection.read);
        self.disown(connection.write);
        let shut = connection.socket.shutdown();
        for (recv, _) in connection.recvs {
            self.ready.push(Completion { tag: recv, done: Done::Read(Vec::new()) });
        }
        for (send, ..) in connection.sends {
            let done = Done::Failed(IoError(format!("{conn:?} hung up before all of it was sent")));
            self.ready.push(Completion { tag: send, done });
        }
        shut?;
        self.ready.push(Completion { tag, done: Done::HungUp });
        Ok(())
    }

    /// Makes the changes asked for and waits up to `left` (forever for `None`), or until the
    /// next timer is due, for events, and acts on them; then fires the timers due.
    fn wait(&mut self, left: Option<Duration>) {
        let now = wheel::ticks(self.start.elapsed());
        let until_timer = self.timers.next_deadline().map(|due| Duration::from_nanos(due.saturating_sub(now)));
        let left = match (left, until_timer) {
            (Some(left), Some(until_timer)) => Some(left.min(until_timer)),
            (left, until_timer) => left.or(until_timer),
        };
        self.wait_events(left);
        let ready = &mut self.ready;
        self.timers.advance(wheel::ticks(self.start.elapsed()), |_, tag| {
            ready.push(Completion { tag, done: Done::Fired });
        });
    }

    fn wait_events(&mut self, left: Option<Duration>) {
        let timeout = left.map(|it| libc::timespec {
            tv_sec: libc::time_t::try_from(it.as_secs()).unwrap_or(libc::time_t::MAX),
            tv_nsec: it.subsec_nanos().into(),
        });
        let timeout = timeout.as_ref().map_or(std::ptr::null(), |it| it as *const libc::timespec);
        let mut changes = std::mem::take(&mut self.changes);
        let mut events = std::mem::take(&mut self.events);
        // Room for a failure of every change besides the events: a failure with no room for it
        // would fail the whole call.
        let room = changes.len() + EVENTS;
        events.clear();
        events.reserve(room);
        // SAFETY: the changes and the room for events laid out as `kevent`s, and a timeout that
        // outlives the call.
        let got = unsafe {
            libc::kevent(
                self.notify.kq.as_raw_fd(),
                changes.as_ptr().cast(),
                changes.len() as libc::c_int,
                events.as_mut_ptr().cast(),
                room as libc::c_int,
                timeout,
            )
        };
        // Made before the call waits, interrupted or not.
        changes.clear();
        self.changes = changes;
        if got < 0 {
            let error = io::Error::last_os_error();
            assert!(error.kind() == io::ErrorKind::Interrupted, "waiting on the kqueue: {error}");
            self.events = events;
            return;
        }
        // SAFETY: the call wrote `got` events.
        unsafe { events.set_len(got as usize) };
        for event in &events {
            let id = event.udata as u64;
            if id == TRIGGER {
                continue;
            }
            // Of a registration that has ended.
            let Some(&owner) = self.owners.get(&id) else { continue };
            if event.flags & libc::EV_ERROR != 0 {
                if event.data != 0 {
                    self.refused(id, owner, io::Error::from_raw_os_error(event.data as i32));
                }
                continue;
            }
            match owner {
                Owner::Exit(tag) => {
                    self.disown(id);
                    if let Some(spawned) = self.spawns.get_mut(&tag) {
                        spawned.exited = true;
                        spawned.stdin = None;
                    }
                    self.finish_spawn(tag);
                }
                Owner::Output { tag, err } => self.read_output(tag, err),
                Owner::Input(tag) => self.write_input(tag),
                Owner::Listen(tag) => self.accept(tag),
                Owner::Connecting(tag) => {
                    self.disown(id);
                    self.ended_connecting(tag);
                }
                Owner::Read(conn) => self.serve_recvs(conn),
                Owner::Write(conn) => self.serve_sends(conn),
            }
        }
        self.events = events;
    }

    /// The kqueue refused a change of `id`'s.
    fn refused(&mut self, id: u64, owner: Owner, error: io::Error) {
        let failed = |error: &io::Error| Done::Failed(IoError(error.to_string()));
        match owner {
            Owner::Exit(tag) => {
                self.disown(id);
                let Some(spawned) = self.spawns.get_mut(&tag) else { return };
                // `ESRCH`: exited already, a child not reaped, which the kernel no longer
                // follows. Anything else: its end cannot be heard, so it does not run on.
                if error.raw_os_error() != Some(libc::ESRCH) {
                    process::kill(spawned.pid);
                    if let Err(waiting) = process::wait_exited(spawned.pid) {
                        spawned.failed.get_or_insert(waiting);
                    }
                    spawned.failed.get_or_insert(error);
                }
                spawned.exited = true;
                spawned.stdin = None;
                self.finish_spawn(tag);
            }
            // Its output or input cannot be moved: it does not run on.
            Owner::Output { tag, err } => {
                let Some(spawned) = self.spawns.get_mut(&tag) else { return };
                process::kill(spawned.pid);
                spawned.failed.get_or_insert(error);
                match err {
                    true => spawned.stderr = None,
                    false => spawned.stdout = None,
                }
                self.finish_spawn(tag);
            }
            Owner::Input(tag) => {
                let Some(spawned) = self.spawns.get_mut(&tag) else { return };
                process::kill(spawned.pid);
                spawned.failed.get_or_insert(error);
                spawned.stdin = None;
            }
            Owner::Listen(tag) => {
                let Some(listening) = self.listens.remove(&tag) else { return };
                self.disown(listening.id);
                let done = Done::Failed(IoError(format!("listening: {error}")));
                self.ready.push(Completion { tag, done });
            }
            Owner::Connecting(tag) => {
                self.disown(id);
                let Some(connecting) = self.connecting.remove(&tag) else { return };
                let done = Done::Failed(IoError(format!("connecting to {:?}: {error}", connecting.at)));
                self.ready.push(Completion { tag, done });
            }
            Owner::Read(conn) => {
                let Some(connection) = self.conns.get_mut(&conn) else { return };
                if let Some((tag, _)) = connection.recvs.pop_front() {
                    self.ready.push(Completion { tag, done: failed(&error) });
                }
                self.serve_recvs(conn);
            }
            Owner::Write(conn) => {
                let Some(connection) = self.conns.get_mut(&conn) else { return };
                if let Some((tag, ..)) = connection.sends.pop_front() {
                    self.ready.push(Completion { tag, done: failed(&error) });
                }
                self.serve_sends(conn);
            }
        }
    }

    fn take(&mut self, out: &mut Vec<Completion>) {
        out.append(&mut self.ready);
        while let Ok(completion) = self.done.try_recv() {
            out.push(completion);
        }
    }
}

impl Drop for Kqueue {
    fn drop(&mut self) {
        for (_, stream) in self.watches.drain() {
            stream.stop();
        }
        for (_, spawned) in self.spawns.drain() {
            abandon(spawned);
        }
        self.work = None;
        for worker in self.workers.drain(..) {
            _ = worker.join();
        }
    }
}

impl Driver for Kqueue {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            let tag = effect.tag();
            match effect {
                Effect::Kill { tag } => {
                    if let Some(spawned) = self.spawns.get(&tag) {
                        process::kill(spawned.pid);
                    }
                }
                Effect::Unwatch { tag } => match self.watches.remove(&tag) {
                    Some(stream) => {
                        stream.stop();
                        // Through the channel, after every change the stream sent.
                        _ = self.notify.done.send(Completion { tag, done: Done::Unwatched });
                    }
                    None => {
                        self.in_flight += 1;
                        let done = Done::Failed(IoError(format!("nothing is watched as {tag:?}")));
                        self.ready.push(Completion { tag, done });
                    }
                },
                // The end of the listen, which is in flight since it was asked for.
                Effect::Unlisten { tag } => match self.listens.remove(&tag) {
                    Some(listening) => {
                        self.disown(listening.id);
                        self.ready.push(Completion { tag, done: Done::Unlistened });
                    }
                    None => {
                        self.in_flight += 1;
                        let done = Done::Failed(IoError(format!("nothing listens as {tag:?}")));
                        self.ready.push(Completion { tag, done });
                    }
                },
                effect => {
                    self.in_flight += 1;
                    if let Err(error) = self.begin(effect) {
                        let done = Done::Failed(IoError(error.to_string()));
                        self.ready.push(Completion { tag, done });
                    }
                }
            }
        }
    }

    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize {
        let before = out.len();
        let deadline = Instant::now().checked_add(wait);
        loop {
            self.take(out);
            if out.len() > before || self.in_flight == 0 {
                // Changes asked for are made now, not once nothing else is ready.
                if !self.changes.is_empty() {
                    self.wait(Some(Duration::ZERO));
                    self.take(out);
                }
                break;
            }
            let left = deadline.map(|it| it.saturating_duration_since(Instant::now()));
            self.wait(left);
            if left.is_some_and(|it| it.is_zero()) {
                self.take(out);
                break;
            }
        }
        let ended = out[before..]
            .iter()
            .filter(|it| {
                !matches!(it.done, Done::Changed { .. } | Done::Journaled(_) | Done::Accepted { .. })
            })
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

/// Runs the file effects this thread takes off `queue`, which every file thread takes from
/// with no lock, until the driver drops its end.
fn work_on(root: &Path, queue: &crossbeam_channel::Receiver<Effect>, notify: &Notify) {
    while let Ok(effect) = queue.recv() {
        let tag = effect.tag();
        let done = match effect {
            Effect::Map { file, .. } => fs::map(&root.join(file.as_str())).map(Done::Mapped),
            Effect::Stamp { files, .. } => fs::stamps(root, &files).map(Done::Stamped),
            Effect::Entries { dir, .. } => {
                fs::entries(&root.join(dir.as_str()), &dir).map(Done::Entries)
            }
            effect => crate::threads::run(root, effect),
        };
        let done = done.unwrap_or_else(|error| Done::Failed(IoError(error.to_string())));
        if !notify.send(Completion { tag, done }) {
            return;
        }
    }
}

/// A process the driver no longer follows, killed and reaped.
fn abandon(spawned: Spawned) {
    let Spawned { pid, finish, stdin, stdout, stderr, .. } = spawned;
    drop((stdin, stdout, stderr));
    process::kill(pid);
    _ = finish.exited(pid, Vec::new(), Vec::new());
}

fn no_connection(conn: Conn) -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, format!("no connection {conn:?}"))
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: an open descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A write to a socket whose peer is gone fails rather than raising SIGPIPE.
fn no_sigpipe(fd: RawFd) -> io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: an open socket and an int of the size given.
    let set = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&raw const on).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if set != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// How a nonblocking connect of `fd` ended.
fn socket_error(fd: RawFd) -> io::Result<()> {
    let mut error: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: an open socket and an int of the size given to write.
    let got = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, (&raw mut error).cast(), &mut len)
    };
    if got != 0 {
        return Err(io::Error::last_os_error());
    }
    match error {
        0 => Ok(()),
        error => Err(io::Error::from_raw_os_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString};

    use super::*;
    use crate::{Confined, Exit, FileName, Sandbox, Traced, run_until_idle, threads::Threads};

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    fn dir(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-kq-{what}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn command(program: &str, args: &[&str], stdin: &[u8]) -> Box<Command> {
        Box::new(Command {
            program: program.into(),
            args: args.iter().map(|it| OsString::from(*it)).collect(),
            env: Vec::new(),
            dir: "/".into(),
            stdin: stdin.to_vec(),
            sandbox: Sandbox::Off,
            memory: None,
        })
    }

    /// Runs `effects` to their ends: what each tag ended with.
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
    fn processes_end_with_their_output_code_and_signal() {
        let root = dir("spawn");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        let big: Vec<u8> = (0..1u32 << 20).map(|n| n as u8).collect();
        let script = "cat; echo err >&2; exit 7";
        let limited = Box::new(Command { memory: Some(1 << 30), ..*command("/bin/echo", &["hi"], b"") });
        let mut on = command("/bin/echo", &["hi"], b"");
        on.sandbox = Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: false };
        let seen = run(&mut driver, vec![
            Effect::Spawn { command: command("/bin/sh", &["-c", script], b"hello"), tag: Tag(1) },
            Effect::Spawn { command: command("/bin/sh", &["-c", "kill -TERM $$"], b""), tag: Tag(2) },
            Effect::Spawn { command: command("/bin/sleep", &["60"], b""), tag: Tag(3) },
            Effect::Kill { tag: Tag(3) },
            Effect::Spawn { command: on, tag: Tag(4) },
            Effect::Spawn { command: limited, tag: Tag(5) },
            // More input than a pipe holds, read back while it is written.
            Effect::Spawn { command: command("/bin/cat", &[], &big), tag: Tag(6) },
            Effect::Kill { tag: Tag(99) },
        ]);
        let one = exit(&seen[&1]);
        assert_eq!((one.code, one.signal), (Some(7), None));
        assert_eq!((one.stdout.as_slice(), one.stderr.as_slice()), (&b"hello"[..], &b"err\n"[..]));
        assert!(one.peak_memory > 0);
        let two = exit(&seen[&2]);
        assert_eq!((two.code, two.signal), (None, Some(libc::SIGTERM)));
        let three = exit(&seen[&3]);
        assert_eq!((three.code, three.signal), (None, Some(libc::SIGKILL)));
        assert!(matches!(&seen[&4], Done::Failed(why) if why.0.contains("Confine")), "{:?}", seen[&4]);
        let five = exit(&seen[&5]);
        assert_eq!((five.code, five.stdout.as_slice()), (Some(0), &b"hi\n"[..]));
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
            let mut args: Vec<OsString> = vec![
                "-c".into(),
                r#"printf read > "$0/record"; exec "$@""#.into(),
                scratch.into(),
                command.program.clone().into(),
            ];
            args.extend(command.args.iter().cloned());
            Ok(Confined {
                program: "/bin/sh".into(),
                args,
                env: command.env.clone(),
                dir: command.dir.clone(),
            })
        }

        fn traced(&self, _: &Command, scratch: &Path) -> Result<Traced, String> {
            match std::fs::read(scratch.join("record")) {
                Ok(record) if record == b"read" => Ok(Traced {
                    files: vec![(PathBuf::from("/etc/hosts"), None)],
                    ..Traced::default()
                }),
                other => Err(format!("no record: {other:?}")),
            }
        }
    }

    #[test]
    fn a_sandboxed_process_runs_as_its_confine_says_and_its_trace_comes_back() {
        let root = dir("confine");
        let mut driver = Kqueue::new(&root, 1).unwrap().confined(Arc::new(Recording));
        let mut traced = command("/bin/echo", &["hi"], b"");
        traced.sandbox = Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: true };
        let seen = run(&mut driver, vec![Effect::Spawn { command: traced, tag: Tag(1) }]);
        let exit = exit(&seen[&1]);
        assert_eq!((exit.code, exit.stdout.as_slice()), (Some(0), &b"hi\n"[..]));
        let wanted = Traced { files: vec![(PathBuf::from("/etc/hosts"), None)], ..Traced::default() };
        assert_eq!(exit.traced, Some(wanted));
        // Its scratch directory is gone.
        let left = std::fs::read_dir(root.join(".blaze-spawn")).unwrap().count();
        assert_eq!(left, 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Listens, connects, sends each way, hangs up each end and stops listening.
    fn round_trip(root: &Path, at: Address) {
        let mut driver = Kqueue::new(root, 1).unwrap();
        let mut effects = vec![
            Effect::Listen { at: at.clone(), tag: Tag(1) },
            Effect::Connect { at, tag: Tag(2) },
        ];
        let (mut server, mut client) = (None, None);
        let mut ends = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, effects| {
            match (done.tag.0, done.done) {
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
                (tag @ (4 | 5), Done::Sent)
                | (tag @ (7 | 9), Done::HungUp)
                | (tag @ 1, Done::Unlistened) => ends.push(tag),
                (tag, other) => panic!("{tag}: {other:?}"),
            }
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
        // The socket's name goes with its listener.
        assert!(!root.join("s").exists());
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
        let mut driver = Kqueue::new(&root, 1).unwrap();
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
        let mut driver = Kqueue::new(dir("timers"), 1).unwrap();
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
        let mut driver = Kqueue::new(dir("wait"), 1).unwrap();
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
        let mut driver = Kqueue::new(&root, 2).unwrap();
        let file = name("store/pack");
        let seen = run(&mut driver, vec![Effect::Write {
            file: file.clone(),
            bytes: b"packed".to_vec(),
            tag: Tag(1),
        }]);
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
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The file effects, one after another, end as on the threads driver.
    #[test]
    fn files_end_as_on_threads() {
        let file = name("vfs/journal");
        let script = |driver: &mut dyn Driver| {
            let mut effects =
                vec![Effect::Write { file: file.clone(), bytes: b"one".to_vec(), tag: Tag(1) }];
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
        let kqueue = script(&mut Kqueue::new(&ours, 2).unwrap());
        let threads = script(&mut Threads::new(&theirs, 2));
        assert_eq!(kqueue, threads);
        assert_eq!(kqueue[5], format!("{:?}", Done::Read(b"onet".to_vec())));
        assert_eq!(kqueue[3], format!("{:?}", Done::Read(b"etw".to_vec())));
        assert_eq!(kqueue[6], "stat 4 false");
        assert!(kqueue[7].starts_with("stat ") && kqueue[7].ends_with(" true"), "{}", kqueue[7]);
        assert_eq!(kqueue[14], "failed");
        // Past the end, after the four bytes truncation left: a hole of zeros, then the byte.
        assert_eq!(kqueue[16], format!("{:?}", Done::Read(b"onet\0\0!".to_vec())));
        assert_eq!(kqueue[17], format!("{:?}", Done::Moded));
        assert_eq!(kqueue[18], format!("{:?}", Done::Renamed));
        assert!(kqueue[19].starts_with("listed ") && kqueue[19].contains("moved"), "{}", kqueue[19]);
        assert!(!kqueue[19].contains("journal"), "{}", kqueue[19]);
        assert_eq!((kqueue[20].as_str(), kqueue[21].as_str()), ("failed", "failed"));
        for it in [ours, theirs] {
            std::fs::remove_dir_all(it).unwrap();
        }
    }

    #[test]
    fn a_watch_hears_a_file_written_under_it_then_ends() {
        let root = dir("watch");
        let mut driver = Kqueue::new(&root, 1).unwrap();
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
        assert!(!heard.contains(&name("outside")), "{heard:?}");
        run(&mut driver, Vec::new());
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_flush_is_told_after_the_changes_made_before_it() {
        let root = dir("flush");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        run(&mut driver, vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }]);
        driver.submit(&mut vec![Effect::Watch { dir: name("src"), tag: Tag(7) }]);
        for round in 0..20u64 {
            let file = format!("src/edit-{round}");
            std::fs::write(root.join(&file), b"edited").unwrap();
            let flush = Tag(100 + round);
            driver.submit(&mut vec![Effect::Flush { watch: Tag(7), tag: flush }]);
            let mut heard = Vec::new();
            let mut done = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut flushed = false;
            while !flushed && Instant::now() < deadline {
                driver.poll(&mut done, Duration::from_millis(50));
                for it in done.drain(..) {
                    match it.done {
                        Done::Changed { files, .. } => heard.extend(files),
                        Done::Flushed if it.tag == flush => flushed = true,
                        other => panic!("{other:?}"),
                    }
                }
            }
            assert!(flushed, "round {round}: the flush never came");
            assert!(heard.contains(&name(&file)), "round {round}: {heard:?}");
        }
        let mut effects = vec![Effect::Flush { watch: Tag(8), tag: Tag(9) }, Effect::Unwatch { tag: Tag(7) }];
        let mut refused = false;
        run_until_idle(&mut driver, &mut effects, |it, _| match (it.tag.0, it.done) {
            (9, Done::Failed(_)) => refused = true,
            (7, Done::Changed { .. } | Done::Unwatched) => {}
            other => panic!("{other:?}"),
        });
        assert!(refused);
        assert_eq!(driver.in_flight(), 0);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Polls the journal `tag` until `until` says of a batch, or the deadline: every batch.
    fn journal_until(
        driver: &mut Kqueue,
        tag: Tag,
        mut until: impl FnMut(&crate::Journaled) -> bool,
    ) -> Vec<crate::Journaled> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut batches, mut done) = (Vec::new(), Vec::new());
        while Instant::now() < deadline {
            driver.poll(&mut done, Duration::from_millis(50));
            for it in done.drain(..) {
                assert_eq!(it.tag, tag);
                let Done::Journaled(batch) = it.done else { panic!("{:?}", it.done) };
                let stop = until(&batch);
                batches.push(*batch);
                if stop {
                    return batches;
                }
            }
        }
        panic!("never came: {batches:?}")
    }

    /// Ends the journal `tag`: what it told after the unwatch was asked, before its end.
    fn unwatch(driver: &mut Kqueue, tag: Tag) {
        let mut effects = vec![Effect::Unwatch { tag }];
        run_until_idle(driver, &mut effects, |it, _| match it.done {
            Done::Journaled(_) | Done::Unwatched => assert_eq!(it.tag, tag),
            other => panic!("{other:?}"),
        });
        assert_eq!(driver.in_flight(), 0);
    }

    #[test]
    fn a_journal_resumes_from_its_cursor_through_the_driver() {
        let root = dir("journal");
        let mut driver = Kqueue::new(&root, 1).unwrap();
        run(&mut driver, vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }]);
        driver.submit(&mut vec![Effect::Journal {
            dir: name("src"),
            since: crate::Cursor::None,
            tag: Tag(7),
        }]);
        let batches = journal_until(&mut driver, Tag(7), |it| it.caught_up);
        assert!(matches!(batches[0].cursor, crate::Cursor::FsEvents { .. }), "{batches:?}");
        assert!(!batches[0].caught_up && batches[0].lost.is_empty());
        // A batch does not end it.
        assert_eq!(driver.in_flight(), 1);
        let cursor = batches.last().unwrap().cursor;
        unwatch(&mut driver, Tag(7));
        std::fs::write(root.join("src/while-down.rs"), b"fn f() {}").unwrap();
        std::thread::sleep(Duration::from_secs(1));
        driver.submit(&mut vec![Effect::Journal { dir: name("src"), since: cursor, tag: Tag(8) }]);
        let batches = journal_until(&mut driver, Tag(8), |it| it.caught_up);
        let told: Vec<_> = batches.iter().flat_map(|it| &it.files).collect();
        assert!(told.contains(&&name("src/while-down.rs")), "{batches:?}");
        assert!(batches.iter().all(|it| it.lost.is_empty()), "{batches:?}");
        assert_eq!(driver.in_flight(), 1);
        unwatch(&mut driver, Tag(8));
        // Of another store: lost, the whole directory.
        let other = crate::Cursor::FsEvents { event: 1, volume: [7; 16] };
        driver.submit(&mut vec![Effect::Journal { dir: name("src"), since: other, tag: Tag(9) }]);
        let batches = journal_until(&mut driver, Tag(9), |_| true);
        assert_eq!(batches[0].lost, [(name("src"), crate::Lost::Reset)]);
        unwatch(&mut driver, Tag(9));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stamps_and_entries_are_the_file_systems() {
        let root = dir("stamps");
        let mut driver = Kqueue::new(&root, 2).unwrap();
        run(&mut driver, vec![
            Effect::Write { file: name("src/lib.rs"), bytes: b"fn f() {}".to_vec(), tag: Tag(1) },
            Effect::Write { file: name("src/deep/mod.rs"), bytes: Vec::new(), tag: Tag(2) },
        ]);
        std::os::unix::fs::symlink("lib.rs", root.join("src/link")).unwrap();
        let mut names = crate::Names::default();
        for it in ["src/lib.rs", "src/deep", "src/link", "src/missing", "src/lib.rs/under"] {
            assert!(names.push_str(it));
        }
        let seen = run(&mut driver, vec![
            Effect::Stamp { files: names.clone(), tag: Tag(3) },
            Effect::Entries { dir: name("src"), tag: Tag(4) },
            Effect::Entries { dir: name("missing"), tag: Tag(5) },
        ]);
        let stamps = fs::stamps(&root, &names).unwrap();
        assert_eq!(seen[&3], Done::Stamped(stamps.clone()));
        assert!(stamps[0].is_file() && stamps[0].size == 9, "{stamps:?}");
        assert!(stamps[1].is_dir() && stamps[2].is_link(), "{stamps:?}");
        assert!(stamps[3].is_gone() && stamps[4].is_gone(), "{stamps:?}");
        let entries = fs::entries(&root.join("src"), &name("src")).unwrap();
        assert_eq!(seen[&4], Done::Entries(entries.clone()));
        let named: Vec<_> = entries.iter().map(|(it, _)| it.as_str()).collect();
        assert_eq!(named, ["src/deep", "src/lib.rs", "src/link"]);
        assert!(matches!(seen[&5], Done::Failed(_)));
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }
}
