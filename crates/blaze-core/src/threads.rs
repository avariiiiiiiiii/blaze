//! A [`Driver`] on the real file system: a few threads run blocking calls, and the thread that
//! polls keeps the timers. Portable: the reference the other drivers are tested against. Two
//! appends to one file in flight at once may land in either order, as with any asynchronous
//! driver; a state machine that needs their order waits for the first.
//!
//! A process runs on a thread of its own that writes its input, reads its output and waits for
//! it; a listen on a thread that waits for connections; each receive, send and connect on a
//! thread of its own, since each may wait on a peer for as long as the peer likes.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use rustc_hash::FxHashMap;

use crate::{
    Completion, Confine, Done, Driver, Effect, IoError, Tag, fs,
    wheel::{self, Wheel},
};

pub struct Threads {
    start: Instant,
    root: Arc<Path>,
    work: Option<crossbeam_channel::Sender<Effect>>,
    done: Receiver<Completion>,
    finished: Sender<Completion>,
    workers: Vec<JoinHandle<()>>,
    /// By ticks since `start`.
    timers: Wheel<Tag>,
    watches: FxHashMap<Tag, Watching>,
    /// The watches that are an [`Effect::Journal`] on a system with no journal (Linux), by
    /// tag, with their directory: their changes are told as [`Done::Journaled`].
    #[cfg(target_os = "linux")]
    journals: FxHashMap<Tag, crate::FileName>,
    confine: Option<Arc<dyn Confine>>,
    #[cfg(unix)]
    world: Arc<world::World>,
    #[cfg(unix)]
    listens: FxHashMap<Tag, world::Listening>,
    in_flight: usize,
}

impl Threads {
    /// A driver whose files are under `root`, with `threads` threads for blocking calls.
    pub fn new(root: impl Into<PathBuf>, threads: usize) -> Threads {
        let root: Arc<Path> = root.into().into();
        let (work, queue) = crossbeam_channel::unbounded::<Effect>();
        let (finished, done) = mpsc::channel();
        let workers = (0..threads.max(1))
            .map(|n| {
                let (root, queue, finished) = (root.clone(), queue.clone(), finished.clone());
                std::thread::Builder::new()
                    .name(format!("blaze-io-{n}"))
                    .spawn(move || work_on(&root, &queue, &finished))
                    .expect("starting an IO thread")
            })
            .collect();
        Threads {
            start: Instant::now(),
            root,
            work: Some(work),
            done,
            finished,
            workers,
            timers: Wheel::new(),
            watches: FxHashMap::default(),
            #[cfg(target_os = "linux")]
            journals: FxHashMap::default(),
            confine: None,
            #[cfg(unix)]
            world: Arc::default(),
            #[cfg(unix)]
            listens: FxHashMap::default(),
            in_flight: 0,
        }
    }

    /// The same driver, making the sandbox of each spawn whose sandbox is on with `confine`.
    pub fn confined(mut self, confine: Arc<dyn Confine>) -> Threads {
        self.confine = Some(confine);
        self
    }

    /// The address the TCP listen `listen` was given (its port, where it asked for port 0);
    /// `None` for a Unix socket or a listen not in flight.
    #[cfg(unix)]
    pub fn local_addr(&self, listen: Tag) -> Option<std::net::SocketAddr> {
        self.listens.get(&listen)?.local
    }

    fn fire_timers(&mut self, out: &mut Vec<Completion>) -> usize {
        let before = self.timers.len();
        self.timers.advance(wheel::ticks(self.start.elapsed()), |_, tag| {
            out.push(Completion { tag, done: Done::Fired });
        });
        let fired = before - self.timers.len();
        self.in_flight -= fired;
        fired
    }

    /// Takes a completion off the threads' channel, counting it out of what is in flight unless
    /// a watch or a listen goes on after it.
    fn took(&mut self, completion: Completion, out: &mut Vec<Completion>) {
        #[cfg(target_os = "linux")]
        let completion = match self.journals.get(&completion.tag) {
            Some(dir) => Completion {
                tag: completion.tag,
                done: crate::journal::live(dir, completion.done),
            },
            None => completion,
        };
        match completion.done {
            Done::Changed { .. } | Done::Journaled(_) | Done::Accepted { .. } => {}
            // A watch or a listen ends with its end or a failure.
            _ => {
                if let Some(watching) = self.watches.remove(&completion.tag) {
                    watching.join();
                }
                #[cfg(target_os = "linux")]
                self.journals.remove(&completion.tag);
                #[cfg(unix)]
                if let Some(listening) = self.listens.remove(&completion.tag) {
                    listening.join();
                }
                self.in_flight -= 1;
            }
        }
        out.push(completion);
    }

    /// Says `done` of `tag` as the threads' completions are said.
    fn tell(&mut self, tag: Tag, done: Done) {
        self.in_flight += 1;
        _ = self.finished.send(Completion { tag, done });
    }
}

impl Drop for Threads {
    fn drop(&mut self) {
        self.work = None;
        for (_, watching) in self.watches.drain() {
            watching.stop();
            watching.join();
        }
        #[cfg(unix)]
        {
            for (_, listening) in self.listens.drain() {
                listening.stop();
                listening.join();
            }
            self.world.kill_all();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// Runs the file effects this thread takes off `queue`, which every IO thread takes from with
/// no lock, until the driver drops its end.
fn work_on(
    root: &Path,
    queue: &crossbeam_channel::Receiver<Effect>,
    finished: &Sender<Completion>,
) {
    while let Ok(effect) = queue.recv() {
        let tag = effect.tag();
        let done =
            run(root, effect).unwrap_or_else(|error| Done::Failed(IoError(error.to_string())));
        if finished.send(Completion { tag, done }).is_err() {
            return;
        }
    }
}

/// A file effect done with blocking calls, for this driver's threads and for the drivers that
/// keep their file calls on threads.
pub(crate) fn run(root: &Path, effect: Effect) -> io::Result<Done> {
    let at = |file: &crate::FileName| root.join(file.as_str());
    match effect {
        Effect::ReadAll { file, .. } => match std::fs::read(at(&file)) {
            Ok(bytes) => Ok(Done::Read(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Done::Read(Vec::new())),
            Err(error) => Err(error),
        },
        Effect::Read { file, at: from, len, .. } => {
            let file = File::open(at(&file))?;
            let mut bytes = vec![0; len];
            let mut read = 0;
            while read < len {
                let n = read_at(&file, &mut bytes[read..], from + read as u64)?;
                if n == 0 {
                    break;
                }
                read += n;
            }
            bytes.truncate(read);
            Ok(Done::Read(bytes))
        }
        Effect::Append { file, bytes, .. } => {
            let path = at(&file);
            fs::make_parent(&path)?;
            OpenOptions::new().create(true).append(true).open(path)?.write_all(&bytes)?;
            Ok(Done::Appended)
        }
        Effect::Sync { file, .. } => {
            // `sync_all` is `F_FULLFSYNC` on Apple's systems, where `fsync` leaves the bytes in
            // the drive's cache.
            OpenOptions::new().append(true).create(true).open(at(&file))?.sync_all()?;
            Ok(Done::Synced)
        }
        Effect::Truncate { file, len, .. } => {
            let path = at(&file);
            fs::make_parent(&path)?;
            OpenOptions::new().write(true).create(true).truncate(false).open(path)?.set_len(len)?;
            Ok(Done::Truncated)
        }
        Effect::Write { file, bytes, .. } => {
            fs::write_atomically(&at(&file), &bytes)?;
            Ok(Done::Written)
        }
        Effect::WriteAt { file, at: offset, bytes, .. } => {
            fs::write_at(&at(&file), offset, &bytes)?;
            Ok(Done::Written)
        }
        Effect::Rename { from, to, .. } => {
            fs::rename(&at(&from), &at(&to))?;
            Ok(Done::Renamed)
        }
        Effect::Mode { file, mode, .. } => {
            fs::set_mode(&at(&file), mode)?;
            Ok(Done::Moded)
        }
        Effect::Clone { from, to, .. } => {
            fs::clone(&at(&from), &at(&to))?;
            Ok(Done::Cloned)
        }
        Effect::Link { from, to, .. } => {
            fs::link(&at(&from), &at(&to))?;
            Ok(Done::Linked)
        }
        Effect::Stat { file, .. } => Ok(Done::Stat(fs::stat(&at(&file))?)),
        #[cfg(unix)]
        Effect::Stamp { files, .. } => Ok(Done::Stamped(fs::stamps(root, &files)?)),
        #[cfg(unix)]
        Effect::Entries { dir, .. } => Ok(Done::Entries(fs::entries(&at(&dir), &dir)?)),
        #[cfg(windows)]
        Effect::Stamp { files, .. } => Ok(Done::Stamped(crate::iocp::stamps(root, &files)?)),
        #[cfg(windows)]
        Effect::Entries { dir, .. } => Ok(Done::Entries(crate::iocp::entries(&at(&dir), &dir)?)),
        Effect::List { dir, .. } => Ok(Done::Listed(fs::list(&at(&dir), &dir)?)),
        Effect::Remove { file, .. } => {
            fs::remove(&at(&file))?;
            Ok(Done::Removed)
        }
        Effect::Map { file, .. } => Ok(Done::Mapped(fs::map(&at(&file))?)),
        Effect::Timer { .. }
        | Effect::Watch { .. }
        | Effect::Journal { .. }
        | Effect::Unwatch { .. }
        | Effect::Flush { .. }
        | Effect::Spawn { .. }
        | Effect::Kill { .. }
        | Effect::Listen { .. }
        | Effect::Unlisten { .. }
        | Effect::Connect { .. }
        | Effect::Recv { .. }
        | Effect::Send { .. }
        | Effect::Hangup { .. } => {
            unreachable!("timers, watches, processes and sockets are kept by the driver")
        }
    }
}

#[cfg(unix)]
fn read_at(file: &File, buffer: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, at)
}

#[cfg(windows)]
fn read_at(file: &File, buffer: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, at)
}

/// Processes and sockets: what their threads share with the thread that polls.
#[cfg(unix)]
mod world {
    use std::{
        fs::File,
        io::{Read as _, Write as _},
        os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
        path::Path,
        sync::{
            Arc, Mutex, PoisonError,
            atomic::{AtomicU64, Ordering},
            mpsc::Sender,
        },
        thread::JoinHandle,
    };

    use rustc_hash::FxHashMap;

    use crate::{Address, Command, Completion, Confine, Conn, Done, IoError, Tag, net, process};

    #[derive(Default)]
    pub(super) struct World {
        processes: Mutex<FxHashMap<Tag, Process>>,
        conns: Mutex<FxHashMap<Conn, Arc<net::Socket>>>,
        next_conn: AtomicU64,
    }

    enum Process {
        /// Asked for and not yet started; `killed` once a kill came first.
        Starting { killed: bool },
        /// Started and not yet reaped: its pid is no one else's.
        Running(libc::pid_t),
    }

    fn failed(tag: Tag, why: impl ToString) -> Completion {
        Completion { tag, done: Done::Failed(IoError(why.to_string())) }
    }

    impl World {
        fn conn(&self, socket: net::Socket) -> Conn {
            let conn = Conn(self.next_conn.fetch_add(1, Ordering::Relaxed));
            self.conns.lock().unwrap_or_else(PoisonError::into_inner).insert(conn, Arc::new(socket));
            conn
        }

        fn socket(&self, conn: Conn) -> Option<Arc<net::Socket>> {
            self.conns.lock().unwrap_or_else(PoisonError::into_inner).get(&conn).cloned()
        }

        pub(super) fn spawn(
            self: &Arc<World>,
            root: &Arc<Path>,
            command: Box<Command>,
            confine: Option<Arc<dyn Confine>>,
            tag: Tag,
            finished: Sender<Completion>,
        ) {
            self.processes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(tag, Process::Starting { killed: false });
            let (world, root, told) = (self.clone(), root.clone(), finished.clone());
            let started = std::thread::Builder::new().name("blaze-process".into()).spawn(move || {
                let done = match world.run(&root, command, confine.as_ref(), tag) {
                    Ok(exit) => Done::Exited(Box::new(exit)),
                    Err(error) => Done::Failed(IoError(error.to_string())),
                };
                _ = finished.send(Completion { tag, done });
            });
            if let Err(error) = started {
                self.processes.lock().unwrap_or_else(PoisonError::into_inner).remove(&tag);
                _ = told.send(failed(tag, error));
            }
        }

        fn run(
            &self,
            root: &Path,
            command: Box<Command>,
            confine: Option<&Arc<dyn Confine>>,
            tag: Tag,
        ) -> std::io::Result<crate::Exit> {
            let started = match process::start(root, command, confine) {
                Ok(started) => started,
                Err(error) => {
                    self.processes.lock().unwrap_or_else(PoisonError::into_inner).remove(&tag);
                    return Err(error);
                }
            };
            let pid = started.pid;
            {
                let mut processes = self.processes.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(Process::Starting { killed: true }) = processes.get(&tag) {
                    process::kill(pid);
                }
                processes.insert(tag, Process::Running(pid));
            }
            let process::Started { stdin, stdout, stderr, mut finish, .. } = started;
            let input = std::mem::take(&mut finish.command.stdin);
            let writer = stdin.map(|fd| {
                std::thread::spawn(move || {
                    // A process that ends without reading all of its input is not an error of
                    // the driver's: it is told by how the process ended.
                    _ = File::from(fd).write_all(&input);
                })
            });
            let errors = std::thread::spawn(move || read_all(stderr));
            let output = read_all(stdout);
            let errors = errors.join().unwrap_or_default();
            if let Some(writer) = writer {
                _ = writer.join();
            }
            process::wait_exited(pid)?;
            self.processes.lock().unwrap_or_else(PoisonError::into_inner).remove(&tag);
            finish.exited(pid, output, errors)
        }

        pub(super) fn kill(&self, tag: Tag) {
            let mut processes = self.processes.lock().unwrap_or_else(PoisonError::into_inner);
            match processes.get_mut(&tag) {
                Some(Process::Starting { killed }) => *killed = true,
                Some(Process::Running(pid)) => process::kill(*pid),
                None => {}
            }
        }

        pub(super) fn kill_all(&self) {
            let mut processes = self.processes.lock().unwrap_or_else(PoisonError::into_inner);
            for process in processes.values_mut() {
                match process {
                    Process::Starting { killed } => *killed = true,
                    Process::Running(pid) => process::kill(*pid),
                }
            }
        }

        pub(super) fn connect(
            self: &Arc<World>,
            root: &Arc<Path>,
            at: Address,
            tag: Tag,
            finished: Sender<Completion>,
        ) {
            let (world, root) = (self.clone(), root.clone());
            on_thread("blaze-connect", tag, finished, move || {
                let socket = net::connect(&root, &at)?;
                Ok(Done::Connected { conn: world.conn(socket) })
            });
        }

        pub(super) fn recv(
            &self,
            conn: Conn,
            most: usize,
            tag: Tag,
            finished: Sender<Completion>,
        ) {
            let Some(socket) = self.socket(conn) else {
                _ = finished.send(failed(tag, format!("no connection {conn:?}")));
                return;
            };
            on_thread("blaze-recv", tag, finished, move || {
                let mut bytes = vec![0; most];
                let n = socket.read(&mut bytes)?;
                bytes.truncate(n);
                Ok(Done::Read(bytes))
            });
        }

        pub(super) fn send(
            &self,
            conn: Conn,
            bytes: Vec<u8>,
            tag: Tag,
            finished: Sender<Completion>,
        ) {
            let Some(socket) = self.socket(conn) else {
                _ = finished.send(failed(tag, format!("no connection {conn:?}")));
                return;
            };
            on_thread("blaze-send", tag, finished, move || {
                socket.write_all(&bytes)?;
                Ok(Done::Sent)
            });
        }

        pub(super) fn hangup(&self, conn: Conn) -> Done {
            let socket = self.conns.lock().unwrap_or_else(PoisonError::into_inner).remove(&conn);
            match socket {
                Some(socket) => match socket.shutdown() {
                    Ok(()) => Done::HungUp,
                    Err(error) => Done::Failed(IoError(error.to_string())),
                },
                None => Done::Failed(IoError(format!("no connection {conn:?}"))),
            }
        }

        pub(super) fn listen(
            self: &Arc<World>,
            root: &Path,
            at: &Address,
            tag: Tag,
            finished: Sender<Completion>,
        ) -> std::io::Result<Listening> {
            let listener = net::listen(root, at)?;
            let mut ends = [0; 2];
            // SAFETY: two descriptors written by the call.
            if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `pipe` succeeded, so `ends[0]` is a new descriptor nothing else owns; owned
            // once, here.
            let read_end = unsafe { OwnedFd::from_raw_fd(ends[0]) };
            // SAFETY: as above, for `ends[1]`.
            let write_end = unsafe { OwnedFd::from_raw_fd(ends[1]) };
            net::set_cloexec(read_end.as_raw_fd())?;
            net::set_cloexec(write_end.as_raw_fd())?;
            listener.set_nonblocking(true)?;
            let local = listener.local_addr();
            let world = self.clone();
            let thread = std::thread::Builder::new().name("blaze-listen".into()).spawn(move || {
                loop {
                    let mut fds = [
                        libc::pollfd { fd: listener.fd(), events: libc::POLLIN, revents: 0 },
                        libc::pollfd { fd: read_end.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                    ];
                    // SAFETY: two pollfds of open descriptors.
                    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                    if ready < 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        _ = finished.send(failed(tag, error));
                        return;
                    }
                    if fds[1].revents != 0 {
                        drop(listener);
                        _ = finished.send(Completion { tag, done: Done::Unlistened });
                        return;
                    }
                    match listener.accept() {
                        Ok(socket) => {
                            if let Err(error) = socket.set_nonblocking(false) {
                                _ = finished.send(failed(tag, error));
                                return;
                            }
                            let done = Done::Accepted { conn: world.conn(socket) };
                            if finished.send(Completion { tag, done }).is_err() {
                                return;
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::Interrupted
                                    | std::io::ErrorKind::ConnectionAborted
                            ) => {}
                        Err(error) => {
                            _ = finished.send(failed(tag, error));
                            return;
                        }
                    }
                }
            })?;
            Ok(Listening { stop: write_end, thread, local })
        }
    }

    fn on_thread(
        name: &str,
        tag: Tag,
        finished: Sender<Completion>,
        work: impl FnOnce() -> std::io::Result<Done> + Send + 'static,
    ) {
        let told = finished.clone();
        let started = std::thread::Builder::new().name(name.into()).spawn(move || {
            let done = work().unwrap_or_else(|error| Done::Failed(IoError(error.to_string())));
            _ = finished.send(Completion { tag, done });
        });
        if let Err(error) = started {
            _ = told.send(failed(tag, error));
        }
    }

    fn read_all(fd: OwnedFd) -> Vec<u8> {
        let mut bytes = Vec::new();
        // An error reading the pipe ends what was read: the process's end says how it went.
        _ = File::from(fd).read_to_end(&mut bytes);
        bytes
    }

    /// A listen's thread, and how to stop it.
    pub(super) struct Listening {
        stop: OwnedFd,
        thread: JoinHandle<()>,
        pub(super) local: Option<std::net::SocketAddr>,
    }

    impl Listening {
        pub(super) fn stop(&self) {
            // SAFETY: one byte to the pipe's write end.
            unsafe { libc::write(self.stop.as_raw_fd(), b"x".as_ptr().cast(), 1) };
        }

        pub(super) fn join(self) {
            _ = self.thread.join();
        }
    }
}

/// A watch's thread, and how to stop it or ask it for a flush.
#[cfg(target_os = "linux")]
struct Watching {
    stop: std::os::fd::OwnedFd,
    /// Written once flushes are added to `flushes`.
    wake: std::os::fd::OwnedFd,
    /// The flushes asked for and not yet taken by the thread; `None` once the thread has
    /// ended, so a flush asked after fails rather than waiting for nothing.
    flushes: Arc<std::sync::Mutex<Option<Vec<Tag>>>>,
    thread: JoinHandle<()>,
}

#[cfg(target_os = "linux")]
fn eventfd() -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd as _;
    // SAFETY: no pointers; a descriptor of its own or -1.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call returned a descriptor of its own.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
impl Watching {
    fn start(
        root: &Path,
        dir: &crate::FileName,
        tag: Tag,
        finished: Sender<Completion>,
    ) -> io::Result<Watching> {
        use std::os::fd::AsRawFd as _;
        let mut tree = crate::watch::Tree::new(root, dir, false)?;
        let (stop, wake) = (eventfd()?, eventfd()?);
        let (stop_fd, wake_fd) = (stop.as_raw_fd(), wake.as_raw_fd());
        let flushes = Arc::new(std::sync::Mutex::new(Some(Vec::new())));
        let asked = flushes.clone();
        let thread = std::thread::Builder::new().name("blaze-watch".into()).spawn(move || {
            let mut buffer = vec![0u8; crate::watch::BUFFER];
            let end = watch_thread(&mut tree, &mut buffer, tag, &finished, &asked, [stop_fd, wake_fd]);
            // Flushes asked for and not taken fail before the end, and none is taken after.
            let left = asked.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
            for flush in left.into_iter().flatten() {
                let why = IoError("the watch ended before the flush".into());
                _ = finished.send(Completion { tag: flush, done: Done::Failed(why) });
            }
            if let Some(done) = end {
                _ = finished.send(Completion { tag, done });
            }
        })?;
        Ok(Watching { stop, wake, flushes, thread })
    }

    fn stop(&self) {
        use std::os::fd::AsRawFd as _;
        let one: u64 = 1;
        // SAFETY: eight bytes to an eventfd.
        unsafe { libc::write(self.stop.as_raw_fd(), (&raw const one).cast(), 8) };
    }

    /// Asks the thread for a flush `tag`, which it answers through `finished`.
    fn flush(&self, tag: Tag, finished: &Sender<Completion>) {
        use std::os::fd::AsRawFd as _;
        let mut flushes = self.flushes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(flushes) = flushes.as_mut() else {
            let why = IoError("the watch ended before the flush".into());
            _ = finished.send(Completion { tag, done: Done::Failed(why) });
            return;
        };
        flushes.push(tag);
        let one: u64 = 1;
        // SAFETY: eight bytes to an eventfd.
        unsafe { libc::write(self.wake.as_raw_fd(), (&raw const one).cast(), 8) };
    }

    fn join(self) {
        _ = self.thread.join();
    }
}

/// A watch's thread: reads its events until it is stopped, answering each flush once it has
/// read every event queued when the flush was taken. What ends the watch, or `None` once the
/// driver is gone.
#[cfg(target_os = "linux")]
fn watch_thread(
    tree: &mut crate::watch::Tree,
    buffer: &mut [u8],
    tag: Tag,
    finished: &Sender<Completion>,
    asked: &std::sync::Mutex<Option<Vec<Tag>>>,
    [stop, wake]: [std::os::fd::RawFd; 2],
) -> Option<Done> {
    let mut taken = Vec::new();
    loop {
        let mut fds = [
            libc::pollfd { fd: tree.raw(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: stop, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: three pollfds of open descriptors.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 3, -1) };
        if ready < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Some(Done::Failed(IoError(io::Error::last_os_error().to_string())));
        }
        if fds[2].revents != 0 {
            let mut count = 0u64;
            // SAFETY: eight bytes from an eventfd, which resets it.
            unsafe { libc::read(wake, (&raw mut count).cast(), 8) };
            taken.extend(
                asked
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
                    .map(std::mem::take)
                    .unwrap_or_default(),
            );
            let mut gone = false;
            let drained = tree.drain(buffer, |done| {
                gone |= finished.send(Completion { tag, done }).is_err();
            });
            if gone {
                return None;
            }
            for flush in taken.drain(..) {
                let done = match &drained {
                    Ok(()) => Done::Flushed,
                    Err(error) => Done::Failed(IoError(format!("flushing the watch: {error}"))),
                };
                if finished.send(Completion { tag: flush, done }).is_err() {
                    return None;
                }
            }
            continue;
        }
        if fds[1].revents != 0 {
            return Some(Done::Unwatched);
        }
        if fds[0].revents != 0 {
            // SAFETY: the buffer has room for what is read.
            let n = unsafe { libc::read(tree.raw(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if n > 0 {
                let done = tree.take(&buffer[..n as usize]);
                if finished.send(Completion { tag, done }).is_err() {
                    return None;
                }
            }
        }
    }
}

/// A watch of FSEvents', which calls back on a queue of its own.
#[cfg(target_os = "macos")]
struct Watching {
    stream: std::sync::Mutex<Option<crate::fsevents::Stream>>,
    tag: Tag,
    finished: Sender<Completion>,
}

#[cfg(target_os = "macos")]
impl Watching {
    fn start(
        root: &Path,
        dir: &crate::FileName,
        tag: Tag,
        finished: Sender<Completion>,
    ) -> io::Result<Watching> {
        let send = finished.clone();
        let stream = crate::fsevents::Stream::start(
            root,
            dir,
            tag,
            Box::new(move |completion| _ = send.send(completion)),
        )?;
        Ok(Watching { stream: std::sync::Mutex::new(Some(stream)), tag, finished })
    }

    /// An [`Effect::Journal`]: FSEvents' stream from the event `since` names.
    fn journal(
        root: &Path,
        dir: &crate::FileName,
        since: crate::Cursor,
        tag: Tag,
        finished: Sender<Completion>,
    ) -> io::Result<Watching> {
        let send = finished.clone();
        let stream = crate::fsevents::Stream::journal(
            root,
            dir,
            since,
            tag,
            Box::new(move |completion| _ = send.send(completion)),
        )?;
        Ok(Watching { stream: std::sync::Mutex::new(Some(stream)), tag, finished })
    }

    fn stop(&self) {
        let stream = self.stream.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(stream) = stream {
            stream.stop();
            _ = self.finished.send(Completion { tag: self.tag, done: Done::Unwatched });
        }
    }

    /// Asks the stream for a flush `tag`, which it answers through `finished`.
    fn flush(&self, tag: Tag, finished: &Sender<Completion>) {
        let stream = self.stream.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let failed = match stream.as_ref() {
            Some(stream) => stream.flush(tag).err().map(|error| format!("flushing the watch: {error}")),
            None => Some("the watch ended before the flush".to_owned()),
        };
        if let Some(why) = failed {
            _ = finished.send(Completion { tag, done: Done::Failed(IoError(why)) });
        }
    }

    fn join(self) {}
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct Watching;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl Watching {
    fn start(
        _: &Path,
        _: &crate::FileName,
        _: Tag,
        _: Sender<Completion>,
    ) -> io::Result<Watching> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "watching files is built on Linux (inotify, fanotify) and macOS (FSEvents) only",
        ))
    }

    fn journal(
        _: &Path,
        _: &crate::FileName,
        _: crate::Cursor,
        _: Tag,
        _: Sender<Completion>,
    ) -> io::Result<Watching> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the threads driver reads journals of changes on Linux and macOS only; on Windows the \
             USN journal is read by the IOCP driver",
        ))
    }

    fn stop(&self) {}

    fn flush(&self, _: Tag, _: &Sender<Completion>) {
        unreachable!("no watch starts here")
    }

    fn join(self) {}
}

/// Why a flush of a watch not in flight fails.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn no_watch(watch: Tag) -> String {
    format!("nothing is watched as {watch:?}")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn no_watch(watch: Tag) -> String {
    format!(
        "nothing is watched as {watch:?}: watching files, and so flushing a watch, is built on \
         Linux (inotify, fanotify) and macOS (FSEvents) only"
    )
}

impl Driver for Threads {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            match effect {
                Effect::Timer { after, tag } => {
                    self.in_flight += 1;
                    let due = wheel::ticks(self.start.elapsed()).saturating_add(wheel::ticks(after));
                    self.timers.insert(due, tag);
                }
                Effect::Watch { dir, tag } => {
                    match Watching::start(&self.root, &dir, tag, self.finished.clone()) {
                        Ok(watching) => {
                            self.in_flight += 1;
                            self.watches.insert(tag, watching);
                        }
                        Err(error) => self.tell(tag, Done::Failed(IoError(error.to_string()))),
                    }
                }
                #[cfg(target_os = "linux")]
                Effect::Journal { dir, since, tag } => {
                    match Watching::start(&self.root, &dir, tag, self.finished.clone()) {
                        Ok(watching) => {
                            self.in_flight += 1;
                            self.watches.insert(tag, watching);
                            let first = crate::journal::first_on_linux(&dir, &since);
                            self.journals.insert(tag, dir);
                            _ = self.finished.send(Completion { tag, done: first });
                        }
                        Err(error) => self.tell(tag, Done::Failed(IoError(error.to_string()))),
                    }
                }
                #[cfg(not(target_os = "linux"))]
                Effect::Journal { dir, since, tag } => {
                    match Watching::journal(&self.root, &dir, since, tag, self.finished.clone()) {
                        Ok(watching) => {
                            self.in_flight += 1;
                            self.watches.insert(tag, watching);
                        }
                        Err(error) => self.tell(tag, Done::Failed(IoError(error.to_string()))),
                    }
                }
                Effect::Unwatch { tag } => match self.watches.get(&tag) {
                    Some(watching) => watching.stop(),
                    None => self.tell(
                        tag,
                        Done::Failed(IoError(format!("nothing is watched as {tag:?}"))),
                    ),
                },
                Effect::Flush { watch, tag } => match self.watches.get(&watch) {
                    Some(watching) => {
                        self.in_flight += 1;
                        watching.flush(tag, &self.finished);
                    }
                    None => self.tell(tag, Done::Failed(IoError(no_watch(watch)))),
                },
                #[cfg(unix)]
                Effect::Spawn { command, tag } => {
                    self.in_flight += 1;
                    self.world.spawn(
                        &self.root,
                        command,
                        self.confine.clone(),
                        tag,
                        self.finished.clone(),
                    );
                }
                #[cfg(unix)]
                Effect::Kill { tag } => self.world.kill(tag),
                #[cfg(unix)]
                Effect::Listen { at, tag } => {
                    match self.world.listen(&self.root, &at, tag, self.finished.clone()) {
                        Ok(listening) => {
                            self.in_flight += 1;
                            self.listens.insert(tag, listening);
                        }
                        Err(error) => self.tell(tag, Done::Failed(IoError(error.to_string()))),
                    }
                }
                #[cfg(unix)]
                Effect::Unlisten { tag } => match self.listens.get(&tag) {
                    Some(listening) => listening.stop(),
                    None => self.tell(
                        tag,
                        Done::Failed(IoError(format!("nothing is listened on as {tag:?}"))),
                    ),
                },
                #[cfg(unix)]
                Effect::Connect { at, tag } => {
                    self.in_flight += 1;
                    self.world.connect(&self.root, at, tag, self.finished.clone());
                }
                #[cfg(unix)]
                Effect::Recv { conn, most, tag } => {
                    self.in_flight += 1;
                    self.world.recv(conn, most, tag, self.finished.clone());
                }
                #[cfg(unix)]
                Effect::Send { conn, bytes, tag } => {
                    self.in_flight += 1;
                    self.world.send(conn, bytes, tag, self.finished.clone());
                }
                #[cfg(unix)]
                Effect::Hangup { conn, tag } => {
                    let done = self.world.hangup(conn);
                    self.tell(tag, done);
                }
                #[cfg(not(unix))]
                Effect::Kill { .. } => {}
                #[cfg(not(unix))]
                effect @ (Effect::Spawn { .. }
                | Effect::Listen { .. }
                | Effect::Unlisten { .. }
                | Effect::Connect { .. }
                | Effect::Recv { .. }
                | Effect::Send { .. }
                | Effect::Hangup { .. }) => self.tell(
                    effect.tag(),
                    Done::Failed(IoError(
                        "processes and sockets are built on Unix only; Windows' are lane \
                         usercode's (AppContainer and job objects)"
                            .into(),
                    )),
                ),
                effect => {
                    self.in_flight += 1;
                    let work = self.work.as_ref().expect("the driver is running");
                    work.send(effect).expect("the IO threads are running");
                }
            }
        }
    }

    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize {
        let before = out.len();
        self.fire_timers(out);
        if out.len() == before {
            let now = wheel::ticks(self.start.elapsed());
            let until_timer =
                self.timers.next_deadline().map(|at| Duration::from_nanos(at.saturating_sub(now)));
            let wait = until_timer.map_or(wait, |it| it.min(wait));
            match self.done.recv_timeout(wait) {
                Ok(completion) => self.took(completion, out),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return out.len() - before,
            }
            self.fire_timers(out);
        }
        while let Ok(completion) = self.done.try_recv() {
            self.took(completion, out);
        }
        out.len() - before
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }
}
