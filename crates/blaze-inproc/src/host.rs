//! The daemon's side of a run: the executable written to cargo's path, the command that
//! spawns it through `blaze-core`'s `Effect::Spawn`, and the handshake that lays the code
//! memory over it before `main` (the crate docs' "The start of a run").
//!
//! A [`Host`] listens on a socket of its own (a named pipe on Windows) in a directory only
//! its user can reach. [`Host::launch`] makes the command, with the socket's address and a
//! random token in `BLAZE_INPROC`, and a [`Pending`] that becomes the run's [`Session`] once
//! the start hook connected from the process the OS says it is, said the token, was given the
//! memory and laid it over its image. Each connection's handshake runs on a thread of the
//! host: no plane worker blocks on a run.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime};

use blaze_core::{Command, Sandbox};

use crate::channel::Channel;
use crate::frame::{Reader, Writer, kinds};
use crate::handshake::{Grant, HOST_VAR, Hello, Overlay, Place, VERSION, decode_map, hex};
use crate::sys::{self, Rights};
use crate::{Access, CodeMemory, CodeRange, Error, FixedMap};

/// How long a start hook may take between its frames during the handshake.
const HANDSHAKE: Duration = Duration::from_secs(30);

/// The executable a run starts: `len` bytes of the image part of its code memory, the file the
/// linker wrote, to go at cargo's `path`. `key` names those bytes (the store's content hash of
/// the link's output); `store`, when given, is the store's file of exactly them, verified by
/// the store against `key`, which a filesystem with clones shares blocks with.
pub struct Executable<'a> {
    pub path: &'a Path,
    pub key: [u8; 32],
    pub len: u64,
    pub store: Option<&'a Path>,
}

/// What [`Host::write_executable`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wrote {
    /// The file at the path is the one written for this key, unchanged since: nothing.
    Kept,
    /// A clone of the store's file: no bytes copied.
    Cloned,
    /// The bytes, once.
    Written,
}

/// What a written file was when it was written: a file changed since (a rebuild by another
/// tool, `cargo clean`) is written again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Identity {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64),
}

impl Identity {
    fn of(path: &Path) -> Option<Identity> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        Some(Identity {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino())
            },
        })
    }
}

/// A run to start: `program`, the executable written from `image`'s code memory
/// ([`Host::write_executable`]), with the `maps` laid over its reservations too.
pub struct Launch<'a> {
    pub image: &'a CodeMemory,
    pub maps: Vec<FixedMap<'a>>,
    pub program: PathBuf,
    /// The program's arguments after `argv[0]`, which is `program`.
    pub args: Vec<OsString>,
    /// The program's whole environment.
    pub env: Vec<(OsString, OsString)>,
    pub dir: PathBuf,
    pub stdin: Vec<u8>,
    pub sandbox: Sandbox,
    pub memory: Option<u64>,
}

/// What waits for a start hook to connect, by token.
struct Waiting {
    objects: Vec<(sys::Object, Rights)>,
    grant: Grant,
    encoded: Vec<u8>,
    done: mpsc::Sender<Result<Session, Error>>,
}

/// The launches whose start hook has not connected yet. Locked only to add, take or drop one
/// entry; nothing is called with it held.
type Table = Arc<Mutex<HashMap<[u8; 16], Waiting>>>;

pub struct Host {
    address: String,
    waiting: Table,
    /// The executables written, by path: the key and the file as written.
    written: Mutex<HashMap<PathBuf, ([u8; 32], Identity)>>,
    temporaries: AtomicU64,
}

impl Host {
    /// A host listening in `dir` (made if missing, reachable by this user only).
    pub fn new(dir: &Path) -> Result<Host, Error> {
        let (listener, address) = sys::Listener::bind(dir)?;
        let waiting: Table = Arc::default();
        let table = waiting.clone();
        std::thread::Builder::new()
            .name("blaze-inproc host".to_owned())
            .spawn(move || accept(listener, table))
            .map_err(|it| Error::new(format!("starting the host's thread: {it}")))?;
        Ok(Host { address, waiting, written: Mutex::default(), temporaries: AtomicU64::new(0) })
    }

    /// Puts `executable` at its path from `memory`: nothing when this host wrote the same key
    /// there and the file is unchanged since, else a clone of the store's file or the bytes
    /// of the image part, into a new file renamed over the path (a process still running the
    /// old one keeps it). The bytes are the image part's only while it is pristine
    /// ([`CodeMemory::is_pristine`]): a memory patched since is an error, never a file that
    /// differs from what the overlay swaps in.
    pub fn write_executable(&self, memory: &CodeMemory, executable: &Executable<'_>) -> Result<Wrote, Error> {
        let path = executable.path;
        let mut written = self.written.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((key, identity)) = written.get(path)
            && *key == executable.key
            && Identity::of(path) == Some(*identity)
        {
            return Ok(Wrote::Kept);
        }
        if !memory.is_pristine() {
            return Err(Error::new(format!(
                "writing {}: the code memory's image part changed since it was laid out, so it is no longer the file the \
                 linker wrote",
                path.display()
            )));
        }
        if executable.len == 0 || executable.len > memory.image_len() {
            return Err(Error::new(format!(
                "writing {}: an executable of {:#x} bytes from an image part of {:#x}",
                path.display(),
                executable.len,
                memory.image_len()
            )));
        }
        let bytes = memory.bytes(CodeRange { offset: 0, len: executable.len })?;
        let io = |what: &str, at: &Path, it: std::io::Error| Error::new(format!("{what} {}: {it}", at.display()));
        let name = path.file_name().ok_or_else(|| Error::new(format!("an executable path {} with no file name", path.display())))?;
        let mut temporary = name.to_owned();
        temporary.push(format!(
            ".blaze-{}-{}.tmp",
            std::process::id(),
            self.temporaries.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary = path.with_file_name(temporary);
        let wrote = match executable.store {
            Some(store) if sys::clone_file(store, &temporary)? => {
                let cloned = std::fs::read(&temporary).map_err(|it| io("reading the clone", &temporary, it))?;
                if cloned != bytes {
                    let _ = std::fs::remove_file(&temporary);
                    return Err(Error::new(format!(
                        "the store's file {} for {} is not the image the code memory holds",
                        store.display(),
                        path.display()
                    )));
                }
                Wrote::Cloned
            }
            _ => {
                std::fs::write(&temporary, bytes).map_err(|it| io("writing", &temporary, it))?;
                Wrote::Written
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))
                .map_err(|it| io("making executable", &temporary, it))?;
        }
        if let Err(error) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(io("renaming the new executable over", path, error));
        }
        let identity = Identity::of(path).ok_or_else(|| Error::new(format!("{} is gone right after it was written", path.display())))?;
        written.insert(path.to_owned(), (executable.key, identity));
        Ok(wrote)
    }

    /// The command that starts `launch`, for an `Effect::Spawn`, and what becomes its
    /// session.
    pub fn launch(&self, launch: &Launch<'_>) -> Result<(Command, Pending), Error> {
        let (objects, grant) = grant(launch)?;
        let mut encoded = Vec::new();
        grant.encode(&mut encoded);

        let mut token = [0u8; 16];
        sys::random(&mut token)?;
        let (done, wait) = mpsc::channel();
        self.table().insert(token, Waiting { objects, grant, encoded, done });

        let mut var = OsString::from(&self.address);
        var.push("\t");
        var.push(hex(&token));
        let mut env: Vec<(OsString, OsString)> = launch.env.iter().filter(|(name, _)| name != HOST_VAR).cloned().collect();
        env.push((HOST_VAR.into(), var));
        let mut sandbox = launch.sandbox.clone();
        if let Sandbox::On { read, write, .. } = &mut sandbox {
            read.push(launch.program.clone());
            // Connecting to a Unix socket is a write of the socket alone: the program gets
            // neither the directory nor the other runs' sockets in it. A named pipe is not a
            // path the sandbox's file rules cover.
            if cfg!(unix) {
                write.push(PathBuf::from(&self.address));
            }
        }
        let command = Command {
            program: launch.program.clone(),
            args: launch.args.clone(),
            env,
            dir: launch.dir.clone(),
            stdin: launch.stdin.clone(),
            sandbox,
            memory: launch.memory,
        };
        Ok((command, Pending { token, wait, waiting: self.waiting.clone() }))
    }

    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 16], Waiting>> {
        self.waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The objects a run of `launch` is given and its grant: every code segment from the image
/// part, the arena, the cells and the fixed maps in their reservations.
fn grant(launch: &Launch<'_>) -> Result<(Vec<(sys::Object, Rights)>, Grant), Error> {
    let memory = launch.image;
    let layout = memory
        .layout()
        .ok_or_else(|| Error::new("a launch of a code memory not laid out (CodeMemory::lay_out)"))?;
    let mut objects = vec![(memory.object().duplicate()?, Rights::Code), (memory.control().duplicate()?, Rights::Words)];
    let mut overlays: Vec<Overlay> = layout
        .code
        .iter()
        .map(|it| Overlay { object: 0, offset: it.offset, len: it.len, at: it.linked, access: Access::Execute, place: Place::Code })
        .collect();
    if let Some((range, at)) = memory.arena_part()
        && range.len != 0
    {
        overlays.push(Overlay { object: 0, offset: range.offset, len: range.len, at, access: Access::Execute, place: Place::Reserved });
    }
    if let (Some(object), Some((range, at))) = (memory.cells_object(), memory.cells_part()) {
        objects.push((object.duplicate()?, Rights::Words));
        overlays.push(Overlay {
            object: (objects.len() - 1) as u32,
            offset: 0,
            len: range.len,
            at,
            access: Access::Shared,
            place: Place::Reserved,
        });
    }
    let mut seen: Vec<(*const CodeMemory, u32)> = Vec::new();
    for map in &launch.maps {
        let page = map.memory.page();
        if !(map.at | map.range.offset | map.range.len).is_multiple_of(page) || map.range.len == 0 {
            return Err(Error::new(format!(
                "a fixed map of {:#x} bytes at {:#x} from offset {:#x}: not whole pages of {page:#x}",
                map.range.len, map.at, map.range.offset
            )));
        }
        if map.access == Access::Shared {
            return Err(Error::new(format!("a fixed map at {:#x} shared writable", map.at)));
        }
        if map.range.end() > map.memory.cells().offset {
            return Err(Error::new(format!(
                "a fixed map of {:#x}..{:#x}: past its memory's image and arena",
                map.range.offset,
                map.range.end()
            )));
        }
        let memory = map.memory as *const CodeMemory;
        let object = match seen.iter().find(|(it, _)| *it == memory) {
            Some(&(_, object)) => object,
            None => {
                objects.push((map.memory.object().duplicate()?, Rights::Code));
                let object = (objects.len() - 1) as u32;
                seen.push((memory, object));
                object
            }
        };
        overlays.push(Overlay {
            object,
            offset: map.range.offset,
            len: map.range.len,
            at: map.at,
            access: map.access,
            place: Place::Reserved,
        });
    }
    overlays.sort_by_key(|it| it.at);
    for pair in overlays.windows(2) {
        if pair[0].range().end > pair[1].at {
            return Err(Error::new(format!(
                "overlays at {:#x} and {:#x} overlap: a fixed map over the arena, the cells or another map",
                pair[0].at, pair[1].at
            )));
        }
    }
    for overlay in &overlays {
        let reserved = layout.reserved.iter().any(|it| it.start <= overlay.at && overlay.range().end <= it.end);
        if overlay.place == Place::Reserved && !reserved {
            return Err(Error::new(format!(
                "an overlay of {:#x} bytes at {:#x}: not inside a reservation of the executable",
                overlay.len, overlay.at
            )));
        }
    }
    let grant = Grant {
        format: layout.format,
        header: layout.header,
        span: layout.span.clone(),
        objects: objects.iter().map(|(object, rights)| (object.len(), *rights)).collect(),
        overlays,
    };
    Ok((objects, grant))
}

/// A launch whose start hook has not finished its handshake.
pub struct Pending {
    token: [u8; 16],
    wait: mpsc::Receiver<Result<Session, Error>>,
    waiting: Table,
}

impl Pending {
    /// Waits until the start hook laid the memory over the image, or `timeout`. Blocking: call
    /// it on a thread of the run's, not a plane worker.
    pub fn wait(self, timeout: Duration) -> Result<Session, Error> {
        let got = self.wait.recv_timeout(timeout);
        self.waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&self.token);
        match got {
            Ok(session) => session,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(Error::new(format!("the start hook did not attach within {timeout:?}")))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::new("the host stopped before the start hook attached")),
        }
    }
}

/// A started run: its process, its slide, and the channel to its runtime.
pub struct Session {
    pub pid: u32,
    /// What the process adds to a linked address ([`CodeMemory::linked`]).
    pub slide: u64,
    /// The thread that runs `main`.
    pub thread: u64,
    channel: Channel,
    /// What acts in the process (Windows maps from here); kept while the run lives.
    _remote: sys::Remote,
}

impl Session {
    pub fn channel(&mut self) -> &mut Channel {
        &mut self.channel
    }
}

/// How long a started process's start hook may take to connect and attach.
const START: Duration = Duration::from_secs(60);

/// A program placed in a code memory, which any number of processes run: what the daemon's
/// dev `run` and test paths hold for an executable. Each [`Program::command`] writes the
/// executable to the command's path when it is not there yet and is a launch of it; a thread
/// of its own waits for the start hook, attaches the process to the memory and hands the
/// session on (to whoever serves the process's requests on its channel: `blaze-lazy`'s
/// runner).
pub struct Program {
    host: Arc<Host>,
    memory: Arc<Mutex<CodeMemory>>,
    image: Image,
    sessions: mpsc::Sender<Result<Session, Error>>,
    starts: Arc<Mutex<Starts>>,
}

/// The executable of a [`Program`], as [`Executable`] names it, for any path.
#[derive(Clone, Debug)]
pub struct Image {
    pub key: [u8; 32],
    pub len: u64,
    pub store: Option<PathBuf>,
}

/// One [`Program::command`]'s process, for [`Program::exited`]: the spawn's completion names
/// no process ID.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Started(u64);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Start {
    Waiting,
    Attached(u32),
    /// Ended before its start hook finished the handshake.
    Ended,
}

#[derive(Default)]
struct Starts {
    next: u64,
    starts: HashMap<u64, Start>,
}

impl Program {
    /// `memory` laid out ([`CodeMemory::lay_out`]), its image part the executable `image`
    /// names.
    pub fn new(host: Arc<Host>, memory: Arc<Mutex<CodeMemory>>, image: Image, sessions: mpsc::Sender<Result<Session, Error>>) -> Program {
        Program { host, memory, image, sessions, starts: Arc::default() }
    }

    pub fn memory(&self) -> &Arc<Mutex<CodeMemory>> {
        &self.memory
    }

    /// The command that runs what `command` would have, its program the executable at its
    /// path: arguments, environment, directory, stdin, sandbox and memory limit stay. `forks`
    /// says the process forks children that run the program (a fork server): see
    /// [`CodeMemory::attach_forking`].
    pub fn command(&self, command: Command, forks: bool) -> Result<(Command, Started), Error> {
        let (command, pending) = {
            let memory = lock(&self.memory);
            let executable = Executable {
                path: &command.program,
                key: self.image.key,
                len: self.image.len,
                store: self.image.store.as_deref(),
            };
            self.host.write_executable(&memory, &executable)?;
            let launch = Launch {
                image: &memory,
                maps: Vec::new(),
                program: command.program,
                args: command.args,
                env: command.env,
                dir: command.dir,
                stdin: command.stdin,
                sandbox: command.sandbox,
                memory: command.memory,
            };
            self.host.launch(&launch)?
        };
        let started = {
            let mut starts = lock_starts(&self.starts);
            starts.next += 1;
            let id = starts.next;
            starts.starts.insert(id, Start::Waiting);
            Started(id)
        };
        let memory = self.memory.clone();
        let starts = self.starts.clone();
        let sessions = self.sessions.clone();
        std::thread::Builder::new()
            .name("blaze-inproc start".to_owned())
            .spawn(move || {
                let session = pending.wait(START).and_then(|session| {
                    let mut starts = lock_starts(&starts);
                    match starts.starts.get(&started.0) {
                        Some(Start::Waiting) => {}
                        _ => {
                            starts.starts.remove(&started.0);
                            return Err(Error::new(format!(
                                "process {} ended while its start hook attached",
                                session.pid
                            )));
                        }
                    }
                    let mut memory = lock(&memory);
                    match forks {
                        true => memory.attach_forking(session.pid),
                        false => memory.attach(session.pid),
                    }
                    starts.starts.insert(started.0, Start::Attached(session.pid));
                    Ok(session)
                });
                if session.is_err() {
                    let mut starts = lock_starts(&starts);
                    match starts.starts.get(&started.0) {
                        Some(Start::Waiting) => {
                            starts.starts.insert(started.0, Start::Ended);
                        }
                        Some(Start::Ended) => {
                            starts.starts.remove(&started.0);
                        }
                        _ => {}
                    }
                }
                let _ = sessions.send(session);
            })
            .map_err(|it| Error::new(format!("starting the thread that waits for a start hook: {it}")))?;
        Ok((command, started))
    }

    /// The process of `started` exited, and every child it forked too: its threads hold no
    /// fence any more.
    pub fn exited(&self, started: Started) {
        let mut starts = lock_starts(&self.starts);
        match starts.starts.get(&started.0).copied() {
            Some(Start::Attached(pid)) => {
                starts.starts.remove(&started.0);
                lock(&self.memory).exited(pid);
            }
            Some(Start::Waiting) => {
                starts.starts.insert(started.0, Start::Ended);
            }
            Some(Start::Ended) => {
                starts.starts.remove(&started.0);
            }
            None => {}
        }
    }
}

fn lock_starts(starts: &Mutex<Starts>) -> std::sync::MutexGuard<'_, Starts> {
    starts.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock(memory: &Mutex<CodeMemory>) -> std::sync::MutexGuard<'_, CodeMemory> {
    memory.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn accept(listener: sys::Listener, waiting: Table) {
    loop {
        let (stream, peer) = match listener.accept() {
            Ok(it) => it,
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
        };
        let waiting = waiting.clone();
        let started = std::thread::Builder::new()
            .name("blaze-inproc handshake".to_owned())
            .spawn(move || handshake(Channel::new(stream), peer, &waiting));
        if let Err(error) = started {
            eprintln!("blaze-inproc: starting a handshake thread: {error}");
        }
    }
}

/// One start hook's handshake. A peer that names no waiting launch is dropped: it gets
/// nothing.
fn handshake(mut channel: Channel, peer: u32, waiting: &Table) {
    if channel.set_timeout(Some(HANDSHAKE)).is_err() {
        return;
    }
    let hello = match channel.recv().and_then(|frame| match frame.kind {
        kinds::HELLO => Hello::decode(frame.body),
        other => Err(Error::new(format!("a start hook's first frame of kind {:#06x}", other.0))),
    }) {
        Ok(it) => it,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };
    let Some(launch) = waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&hello.token) else {
        return;
    };
    let mut seq = 0;
    let session = match serve(&mut channel, peer, &hello, &launch, &mut seq) {
        Ok((remote, thread)) => {
            let _ = channel.set_timeout(None);
            Ok(Session { pid: peer, slide: hello.header.wrapping_sub(launch.grant.header), thread, channel, _remote: remote })
        }
        Err(error) => {
            let mut body = Vec::new();
            Writer::new(&mut body).str(&error.to_string());
            let _ = channel.send(kinds::REFUSED, seq, &body);
            Err(error)
        }
    };
    let _ = launch.done.send(session);
}

/// Serves one start hook until it started, `seq` the sequence number of the last frame it
/// sent: what a refusal answers.
fn serve(channel: &mut Channel, peer: u32, hello: &Hello, launch: &Waiting, seq: &mut u32) -> Result<(sys::Remote, u64), Error> {
    if hello.pid != peer {
        return Err(Error::new(format!(
            "a start hook that says it is process {} on a connection from process {peer}",
            hello.pid
        )));
    }
    if hello.version != VERSION {
        return Err(Error::new(format!("a start hook of version {}, where this blaze speaks {VERSION}", hello.version)));
    }
    if hello.page != sys::page() {
        return Err(Error::new(format!("a start hook of pages of {:#x} bytes, where the daemon's are {:#x}", hello.page, sys::page())));
    }
    let slide = hello.header.wrapping_sub(launch.grant.header);
    let objects: Vec<(&sys::Object, Rights)> = launch.objects.iter().map(|(it, rights)| (it, *rights)).collect();
    let remote = sys::send_objects(channel, peer, &launch.encoded, &objects)?;
    loop {
        let frame = channel.recv()?;
        let (kind, body) = (frame.kind, frame.body.to_vec());
        *seq = frame.seq;
        let seq = *seq;
        let bad = |it: crate::frame::FrameError| Error::new(format!("process {peer}'s start hook: {it}"));
        let mut answer = Vec::with_capacity(8);
        match kind {
            kinds::MAP => match decode_map(&body).and_then(|(object, access)| remote.map(object, access)) {
                Ok(address) => {
                    Writer::new(&mut answer).u64(address);
                    channel.send(kinds::MAPPED, seq, &answer)?;
                }
                Err(error) => {
                    Writer::new(&mut answer).str(&error.to_string());
                    channel.send(kinds::REFUSED, seq, &answer)?;
                }
            },
            kinds::OVERLAY => {
                Reader::new(&body).finish().map_err(bad)?;
                remote.overlay(slide, &launch.grant)?;
                channel.send(kinds::OVERLAID, seq, &[])?;
            }
            kinds::STARTED => {
                let mut read = Reader::new(&body);
                let thread = read.u64().map_err(bad)?;
                read.finish().map_err(bad)?;
                return Ok((remote, thread));
            }
            kinds::FAILED => {
                let why = Reader::new(&body).str().map_err(bad)?.to_owned();
                return Err(Error::new(format!("process {peer}'s start hook could not attach: {why}")));
            }
            other => return Err(Error::new(format!("process {peer}'s start hook sent a frame of kind {:#06x}", other.0))),
        }
    }
}
