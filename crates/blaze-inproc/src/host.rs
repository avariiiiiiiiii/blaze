//! The daemon's side of a run: the command that starts the loader, through `blaze-core`'s
//! `Effect::Spawn`, and the handshake that hands it the memory.
//!
//! A [`Host`] listens on a socket of its own (a named pipe on Windows) in a directory only
//! its user can reach. [`Host::launch`] makes the command, with the socket's address and a
//! random token in `BLAZE_INPROC`, and a [`Pending`] that becomes the run's [`Session`] once
//! the loader connected from the process the OS says it is, said the token, was given the
//! memory and started the program. Each connection's handshake runs on a thread of the host:
//! no plane worker blocks on a loader.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use blaze_core::{Command, Sandbox};

use crate::channel::Channel;
use crate::frame::{Reader, Writer, kinds};
use crate::load::{GRANT_CODE, GRANT_CONTROL, Grant, HOST_VAR, VERSION};
use crate::sys::{self, Rights};
use crate::{Access, CodeMemory, Error, FixedMap, Format, TlsReserve};

/// How long a loader may take between its frames during the handshake.
const HANDSHAKE: Duration = Duration::from_secs(30);

/// A run to start: the program's image in `image`, mapped as `format` lays it out, after the
/// `maps` at their fixed addresses.
pub struct Launch<'a> {
    pub image: &'a CodeMemory,
    pub format: Format,
    pub maps: Vec<FixedMap<'a>>,
    /// The program's `argv[0]` and arguments after it.
    pub argv0: OsString,
    pub args: Vec<OsString>,
    /// The program's whole environment.
    pub env: Vec<(OsString, OsString)>,
    pub dir: PathBuf,
    pub stdin: Vec<u8>,
    pub sandbox: Sandbox,
    pub memory: Option<u64>,
}

/// What waits for a loader to connect, by token.
struct Waiting {
    objects: Vec<(sys::Object, Rights)>,
    grant: Vec<u8>,
    done: mpsc::Sender<Result<Session, Error>>,
}

/// The launches whose loader has not connected yet. Locked only to add, take or drop one
/// entry; nothing is called with it held.
type Table = Arc<Mutex<HashMap<[u8; 16], Waiting>>>;

pub struct Host {
    loader: PathBuf,
    address: String,
    reserve: TlsReserve,
    waiting: Table,
}

impl Host {
    /// A host listening in `dir` (made if missing, reachable by this user only), starting runs
    /// with the loader at `loader`, whose thread-local reserve is `reserve` (what
    /// [`Host::probe`]'s command printed).
    pub fn new(dir: &Path, loader: PathBuf, reserve: TlsReserve) -> Result<Host, Error> {
        let (listener, address) = sys::Listener::bind(dir)?;
        let waiting: Table = Arc::default();
        let table = waiting.clone();
        std::thread::Builder::new()
            .name("blaze-inproc host".to_owned())
            .spawn(move || accept(listener, table))
            .map_err(|it| Error::new(format!("starting the host's thread: {it}")))?;
        Ok(Host { loader, address, reserve, waiting })
    }

    /// The command that prints the loader's thread-local reserve ([`TlsReserve::decode`]),
    /// run once before [`Host::new`]. blaze's own binary: no sandbox.
    pub fn probe(loader: &Path) -> Command {
        Command {
            program: loader.to_owned(),
            args: vec!["--reserve".into()],
            env: Vec::new(),
            dir: loader.parent().map(Path::to_owned).unwrap_or_default(),
            stdin: Vec::new(),
            sandbox: Sandbox::Off,
            memory: None,
        }
    }

    /// The loader's thread-local reserve: what the linker places a program's thread-locals in.
    pub fn reserve(&self) -> TlsReserve {
        self.reserve
    }

    /// The command that starts `launch`, for an `Effect::Spawn`, and what becomes its
    /// session.
    pub fn launch(&self, launch: &Launch<'_>) -> Result<(Command, Pending), Error> {
        let mut objects = vec![
            (launch.image.object().duplicate()?, Rights::Code),
            (launch.image.control().duplicate()?, Rights::Control),
        ];
        let mut maps: Vec<(u32, &FixedMap<'_>)> = Vec::with_capacity(launch.maps.len());
        let mut seen: Vec<*const CodeMemory> = Vec::new();
        for map in &launch.maps {
            if !map.at.is_multiple_of(map.memory.page()) || !map.range.offset.is_multiple_of(map.memory.page()) {
                return Err(Error::new(format!(
                    "a fixed map of {:#x} bytes at {:#x} from offset {:#x}: not on pages of {:#x}",
                    map.range.len,
                    map.at,
                    map.range.offset,
                    map.memory.page()
                )));
            }
            if map.access == Access::Shared {
                return Err(Error::new(format!("a fixed map at {:#x} shared writable", map.at)));
            }
            let memory = map.memory as *const CodeMemory;
            let index = match seen.iter().position(|it| *it == memory) {
                Some(index) => index + 2,
                None => {
                    seen.push(memory);
                    objects.push((map.memory.object().duplicate()?, Rights::Code));
                    objects.len() - 1
                }
            };
            maps.push((index as u32, map));
        }
        let mut grant = Vec::new();
        Grant {
            format: launch.format,
            base: launch.image.base().unwrap_or(0),
            image_len: launch.image.image_len(),
            objects: objects
                .iter()
                .map(|(object, rights)| {
                    (object.len(), match rights {
                        Rights::Code => GRANT_CODE,
                        Rights::Control => GRANT_CONTROL,
                    })
                })
                .collect(),
            maps: maps.iter().map(|(object, map)| (*object, map.range.offset, map.range.len, map.at, map.access)).collect(),
        }
        .encode(&mut grant);

        let mut token = [0u8; 16];
        sys::random(&mut token)?;
        let (done, wait) = mpsc::channel();
        self.table().insert(token, Waiting { objects, grant, done });

        let mut var = OsString::from(&self.address);
        var.push("\t");
        var.push(hex(&token));
        let mut env: Vec<(OsString, OsString)> =
            launch.env.iter().filter(|(name, _)| name != HOST_VAR).cloned().collect();
        env.push((HOST_VAR.into(), var));
        let mut args = Vec::with_capacity(launch.args.len() + 1);
        args.push(launch.argv0.clone());
        args.extend(launch.args.iter().cloned());
        let mut sandbox = launch.sandbox.clone();
        if let Sandbox::On { read, write, .. } = &mut sandbox {
            read.push(self.loader.clone());
            // Connecting to a Unix socket is a write of the socket alone: the program gets
            // neither the directory nor the other runs' sockets in it. A named pipe is not a
            // path the sandbox's file rules cover.
            if cfg!(unix) {
                write.push(PathBuf::from(&self.address));
            }
        }
        let command = Command {
            program: self.loader.clone(),
            args,
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

/// A launch whose loader has not finished its handshake.
pub struct Pending {
    token: [u8; 16],
    wait: mpsc::Receiver<Result<Session, Error>>,
    waiting: Table,
}

impl Pending {
    /// Waits until the loader started the program, or `timeout`. Blocking: call it on a thread
    /// of the run's, not a plane worker.
    pub fn wait(self, timeout: Duration) -> Result<Session, Error> {
        let got = self.wait.recv_timeout(timeout);
        self.waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&self.token);
        match got {
            Ok(session) => session,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(Error::new(format!("the loader did not start the program within {timeout:?}")))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(Error::new("the host stopped before the loader started the program"))
            }
        }
    }
}

/// A started run: its process, where its code memory is, and the channel to its runtime.
pub struct Session {
    pub pid: u32,
    /// Where the image's offset 0 is in the process: give it to [`CodeMemory::attach`].
    pub base: u64,
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

/// How long a started process's loader may take to connect and start the program.
const START: Duration = Duration::from_secs(60);

/// A program placed in a code memory, which any number of processes run: what the daemon's
/// dev `run` and test paths hold where they held an executable's path. Each
/// [`Program::command`] is a launch, spawned as the command it gives; a thread of its own waits
/// for the loader, attaches the process to the memory and hands the session on (to whoever
/// serves the process's requests on its channel: `blaze-lazy`'s runner).
pub struct Program {
    host: Arc<Host>,
    memory: Arc<Mutex<CodeMemory>>,
    format: Format,
    sessions: mpsc::Sender<Result<Session, Error>>,
    starts: Arc<Mutex<Starts>>,
}

/// One [`Program::command`]'s process, for [`Program::exited`]: the spawn's completion names
/// no process ID.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Started(u64);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Start {
    Waiting,
    Attached(u32),
    /// Ended before its loader finished the handshake.
    Ended,
}

#[derive(Default)]
struct Starts {
    next: u64,
    starts: HashMap<u64, Start>,
}

impl Program {
    pub fn new(
        host: Arc<Host>,
        memory: Arc<Mutex<CodeMemory>>,
        format: Format,
        sessions: mpsc::Sender<Result<Session, Error>>,
    ) -> Program {
        Program { host, memory, format, sessions, starts: Arc::default() }
    }

    pub fn memory(&self) -> &Arc<Mutex<CodeMemory>> {
        &self.memory
    }

    /// The command that runs what `command` would have, loaded from the memory: its program
    /// path becomes the program's `argv[0]`, and its arguments, environment, directory, stdin,
    /// sandbox and memory limit stay. `forks` says the process forks children that run the
    /// program (a fork server): see [`CodeMemory::attach_forking`].
    pub fn command(&self, command: Command, forks: bool) -> Result<(Command, Started), Error> {
        let (command, pending) = {
            let memory = lock(&self.memory);
            let launch = Launch {
                image: &memory,
                format: self.format,
                maps: Vec::new(),
                argv0: command.program.into_os_string(),
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
                                "process {} ended while its loader started the program",
                                session.pid
                            )));
                        }
                    }
                    let mut memory = lock(&memory);
                    let attached = match forks {
                        true => memory.attach_forking(session.pid, session.base),
                        false => memory.attach(session.pid, session.base),
                    };
                    match attached {
                        Ok(()) => {
                            starts.starts.insert(started.0, Start::Attached(session.pid));
                            Ok(session)
                        }
                        Err(error) => {
                            starts.starts.remove(&started.0);
                            Err(error)
                        }
                    }
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
            .map_err(|it| Error::new(format!("starting the thread that waits for a loader: {it}")))?;
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

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
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

/// One loader's handshake. A peer that names no waiting launch is dropped: it gets nothing.
fn handshake(mut channel: Channel, peer: u32, waiting: &Table) {
    if channel.set_timeout(Some(HANDSHAKE)).is_err() {
        return;
    }
    let (token, hello) = match read_hello(&mut channel) {
        Ok(it) => it,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };
    let Some(launch) = waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&token) else {
        return;
    };
    let result = serve(&mut channel, peer, hello, &launch).map(|(remote, base, thread)| {
        let _ = channel.set_timeout(None);
        (remote, base, thread)
    });
    let session = result.map(|(remote, base, thread)| Session { pid: peer, base, thread, channel, _remote: remote });
    let _ = launch.done.send(session);
}

struct Hello {
    pid: u32,
}

fn read_hello(channel: &mut Channel) -> Result<([u8; 16], Hello), Error> {
    let frame = channel.recv()?;
    let bad = |it: crate::frame::FrameError| Error::new(format!("a loader's hello: {it}"));
    let mut read = frame.reader(kinds::HELLO).map_err(bad)?;
    let token: [u8; 16] = read.array().map_err(bad)?;
    let pid = read.u32().map_err(bad)?;
    let version = read.u32().map_err(bad)?;
    let _page = read.u64().map_err(bad)?;
    let _reserve = TlsReserve { bias: read.i64().map_err(bad)?, size: read.u64().map_err(bad)?, align: read.u64().map_err(bad)? };
    read.finish().map_err(bad)?;
    if version != VERSION {
        return Err(Error::new(format!("a loader of version {version}, where this blaze speaks {VERSION}")));
    }
    Ok((token, Hello { pid }))
}

fn serve(channel: &mut Channel, peer: u32, hello: Hello, launch: &Waiting) -> Result<(sys::Remote, u64, u64), Error> {
    if hello.pid != peer {
        return Err(Error::new(format!(
            "a loader that says it is process {} on a connection from process {peer}",
            hello.pid
        )));
    }
    let objects: Vec<(&sys::Object, Rights)> = launch.objects.iter().map(|(it, rights)| (it, *rights)).collect();
    let remote = sys::send_objects(channel, peer, &launch.grant, &objects)?;
    loop {
        let frame = channel.recv()?;
        let (kind, seq, body) = (frame.kind, frame.seq, frame.body.to_vec());
        let bad = |it: crate::frame::FrameError| Error::new(format!("process {peer}'s loader: {it}"));
        match kind {
            kinds::MAP => {
                let mut read = Reader::new(&body);
                let object = read.u32().map_err(bad)?;
                let offset = read.u64().map_err(bad)?;
                let len = read.u64().map_err(bad)?;
                let at = read.u64().map_err(bad)?;
                let access = Access::from_tag(read.u8().map_err(bad)?)
                    .ok_or_else(|| Error::new(format!("process {peer}'s loader asked for a map of no access")))?;
                read.finish().map_err(bad)?;
                let mut answer = Vec::with_capacity(8);
                match remote.map(object, offset, len, at, access) {
                    Ok(address) => {
                        Writer::new(&mut answer).u64(address);
                        channel.send(kinds::MAPPED, seq, &answer)?;
                    }
                    Err(error) => {
                        Writer::new(&mut answer).str(&error.to_string());
                        channel.send(kinds::REFUSED, seq, &answer)?;
                    }
                }
            }
            kinds::STARTED => {
                let mut read = Reader::new(&body);
                let base = read.u64().map_err(bad)?;
                let thread = read.u64().map_err(bad)?;
                read.finish().map_err(bad)?;
                return Ok((remote, base, thread));
            }
            kinds::FAILED => {
                let why = Reader::new(&body).str().map_err(bad)?.to_owned();
                return Err(Error::new(format!("process {peer}'s loader could not start the program: {why}")));
            }
            other => return Err(Error::new(format!("process {peer}'s loader sent a frame of kind {:#06x}", other.0))),
        }
    }
}
