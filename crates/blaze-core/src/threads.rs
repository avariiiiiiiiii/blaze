//! A [`Driver`] on the real file system: a few threads run blocking calls, and the thread that
//! polls keeps the timers. Portable, and the default on macOS. Two appends to one file in flight
//! at once may land in either order, as with any asynchronous driver; a state machine that
//! needs their order waits for the first.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs::{File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use rustc_hash::FxHashMap;

use crate::{Completion, Done, Driver, Effect, IoError, Tag, fs};

pub struct Threads {
    start: Instant,
    root: Arc<Path>,
    work: Option<Sender<Effect>>,
    done: Receiver<Completion>,
    finished: Sender<Completion>,
    workers: Vec<JoinHandle<()>>,
    timers: BinaryHeap<Reverse<(Instant, Tag)>>,
    watches: FxHashMap<Tag, Watching>,
    in_flight: usize,
}

impl Threads {
    /// A driver whose files are under `root`, with `threads` threads for blocking calls.
    pub fn new(root: impl Into<PathBuf>, threads: usize) -> Threads {
        let root: Arc<Path> = root.into().into();
        let (work, queue) = mpsc::channel::<Effect>();
        let queue = Arc::new(Mutex::new(queue));
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
            timers: BinaryHeap::new(),
            watches: FxHashMap::default(),
            in_flight: 0,
        }
    }

    fn fire_timers(&mut self, out: &mut Vec<Completion>) -> usize {
        let now = Instant::now();
        let mut fired = 0;
        while let Some(Reverse((at, tag))) = self.timers.peek().copied() {
            if at > now {
                break;
            }
            self.timers.pop();
            self.in_flight -= 1;
            out.push(Completion { tag, done: Done::Fired });
            fired += 1;
        }
        fired
    }

    /// Takes a completion off the threads' channel, counting it out of what is in flight unless
    /// a watch goes on after it.
    fn took(&mut self, completion: Completion, out: &mut Vec<Completion>) {
        match completion.done {
            Done::Changed { .. } => {}
            // A watch ends with its end or a failure.
            _ => {
                if let Some(watching) = self.watches.remove(&completion.tag) {
                    watching.join();
                }
                self.in_flight -= 1;
            }
        }
        out.push(completion);
    }
}

impl Drop for Threads {
    fn drop(&mut self) {
        self.work = None;
        for (_, watching) in self.watches.drain() {
            watching.stop();
            watching.join();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn work_on(root: &Path, queue: &Mutex<Receiver<Effect>>, finished: &Sender<Completion>) {
    loop {
        let next = queue.lock().map(|it| it.recv());
        let Ok(Ok(effect)) = next else { return };
        let tag = effect.tag();
        let done =
            run(root, effect).unwrap_or_else(|error| Done::Failed(IoError(error.to_string())));
        if finished.send(Completion { tag, done }).is_err() {
            return;
        }
    }
}

fn run(root: &Path, effect: Effect) -> io::Result<Done> {
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
        Effect::Clone { from, to, .. } => {
            fs::clone(&at(&from), &at(&to))?;
            Ok(Done::Cloned)
        }
        Effect::Link { from, to, .. } => {
            fs::link(&at(&from), &at(&to))?;
            Ok(Done::Linked)
        }
        Effect::Stat { file, .. } => Ok(Done::Stat(fs::stat(&at(&file))?)),
        Effect::Remove { file, .. } => {
            fs::remove(&at(&file))?;
            Ok(Done::Removed)
        }
        Effect::Timer { .. } | Effect::Watch { .. } | Effect::Unwatch { .. } => {
            unreachable!("timers and watches are kept by the polling thread")
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

/// A watch's thread, and how to stop it.
#[cfg(target_os = "linux")]
struct Watching {
    stop: std::os::fd::OwnedFd,
    thread: JoinHandle<()>,
}

#[cfg(target_os = "linux")]
impl Watching {
    fn start(
        root: &Path,
        dir: &crate::FileName,
        tag: Tag,
        finished: Sender<Completion>,
    ) -> io::Result<Watching> {
        use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
        let mut inotify = crate::watch::Inotify::new(root, dir, false)?;
        // SAFETY: no pointers; a descriptor of its own or -1.
        let stop = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if stop < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call returned a descriptor of its own.
        let stop = unsafe { OwnedFd::from_raw_fd(stop) };
        let stop_fd = stop.as_raw_fd();
        let thread = std::thread::Builder::new().name("blaze-watch".into()).spawn(move || {
            let mut buffer = vec![0u8; crate::watch::BUFFER];
            loop {
                let mut fds = [
                    libc::pollfd { fd: inotify.raw(), events: libc::POLLIN, revents: 0 },
                    libc::pollfd { fd: stop_fd, events: libc::POLLIN, revents: 0 },
                ];
                // SAFETY: two pollfds of open descriptors.
                let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                if ready < 0 {
                    if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    let why = io::Error::last_os_error().to_string();
                    _ = finished.send(Completion { tag, done: Done::Failed(IoError(why)) });
                    return;
                }
                if fds[1].revents != 0 {
                    _ = finished.send(Completion { tag, done: Done::Unwatched });
                    return;
                }
                if fds[0].revents != 0 {
                    // SAFETY: the buffer has room for what is read.
                    let n = unsafe {
                        libc::read(inotify.raw(), buffer.as_mut_ptr().cast(), buffer.len())
                    };
                    if n > 0 {
                        let done = inotify.take(&buffer[..n as usize]);
                        if finished.send(Completion { tag, done }).is_err() {
                            return;
                        }
                    }
                }
            }
        })?;
        Ok(Watching { stop, thread })
    }

    fn stop(&self) {
        use std::os::fd::AsRawFd as _;
        let one: u64 = 1;
        // SAFETY: eight bytes to an eventfd.
        unsafe { libc::write(self.stop.as_raw_fd(), (&raw const one).cast(), 8) };
    }

    fn join(self) {
        _ = self.thread.join();
    }
}

#[cfg(not(target_os = "linux"))]
struct Watching;

#[cfg(not(target_os = "linux"))]
impl Watching {
    fn start(
        _: &Path,
        _: &crate::FileName,
        _: Tag,
        _: Sender<Completion>,
    ) -> io::Result<Watching> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "watching files is built on Linux only (FSEvents is step 1 of docs/v2.md)",
        ))
    }

    fn stop(&self) {}

    fn join(self) {}
}

impl Driver for Threads {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            match effect {
                Effect::Timer { after, tag } => {
                    self.in_flight += 1;
                    self.timers.push(Reverse((Instant::now() + after, tag)))
                }
                Effect::Watch { dir, tag } => {
                    self.in_flight += 1;
                    match Watching::start(&self.root, &dir, tag, self.finished.clone()) {
                        Ok(watching) => _ = self.watches.insert(tag, watching),
                        Err(error) => {
                            let done = Done::Failed(IoError(error.to_string()));
                            _ = self.finished.send(Completion { tag, done });
                        }
                    }
                }
                Effect::Unwatch { tag } => match self.watches.get(&tag) {
                    Some(watching) => watching.stop(),
                    None => {
                        self.in_flight += 1;
                        let done = Done::Failed(IoError(format!("nothing is watched as {tag:?}")));
                        _ = self.finished.send(Completion { tag, done });
                    }
                },
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
            let until_timer = self
                .timers
                .peek()
                .map(|Reverse((at, _))| at.saturating_duration_since(Instant::now()));
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
