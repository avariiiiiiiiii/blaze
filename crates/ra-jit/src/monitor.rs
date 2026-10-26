//! The monitor (`docs/monitor.md`): the daemon keeps a Flecs world of what it, its workers and
//! its clients did, and serves it over Flecs REST on localhost. Each tells it after the fact:
//! a worker from a thread of its own, a client with one write to the daemon's socket once its
//! build is over. Nothing reads it back to decide a build, and an event that is lost only
//! leaves the world less complete.

use std::{
    path::Path,
    sync::{OnceLock, mpsc},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) use blaze_monitor::event;
use blaze_monitor::{Event, Listen, Monitor};

use crate::cargo::daemon::{Envelope, Request, home, socket_in, version};

/// `RA_JIT_MONITOR`: `off`, or the port REST listens on; unset, the first free one from
/// [`blaze_monitor::PORT`].
pub(crate) const VAR: &str = "RA_JIT_MONITOR";

/// The file in the daemon's directory that names where the monitor listens.
pub(crate) const FILE: &str = "monitor";

/// The world, in the daemon.
static WORLD: OnceLock<Monitor> = OnceLock::new();

/// In a worker: the thread that sends the daemon what happened.
static SENDER: OnceLock<mpsc::Sender<Event>> = OnceLock::new();

fn off() -> bool {
    std::env::var(VAR).is_ok_and(|it| it == "off")
}

/// In the daemon: starts the world and its REST server, and writes where it listens to the
/// daemon's directory. The port, when it serves.
pub(crate) fn start(home: &Path) -> Option<u16> {
    let file = home.join(FILE);
    _ = std::fs::remove_file(&file);
    let listen = match std::env::var(VAR) {
        Err(_) => Listen::Scan,
        Ok(it) if it == "off" => return None,
        Ok(it) => match it.parse() {
            Ok(port) => Listen::Port(port),
            Err(_) => {
                eprintln!("monitor: {VAR}=`{it}` is neither a port nor `off`; it is off");
                return None;
            }
        },
    };
    let monitor = match Monitor::start(listen) {
        Ok(monitor) => monitor,
        Err(why) => {
            eprintln!("monitor: not started: {why}");
            return None;
        }
    };
    let port = monitor.port;
    match port {
        Some(port) => {
            _ = std::fs::write(&file, format!("http://127.0.0.1:{port}\n"));
            eprintln!("monitor: Flecs REST on http://127.0.0.1:{port} (docs/monitor.md)");
        }
        None => eprintln!("monitor: no free port on 127.0.0.1 for its REST server"),
    }
    _ = WORLD.set(monitor);
    port
}

/// Where the monitor of the daemon whose directory is `home` listens, as it wrote it.
pub(crate) fn url(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join(FILE)).ok()?;
    Some(text.trim().to_owned()).filter(|it| !it.is_empty())
}

/// Tells the monitor `event`, from a worker or the daemon. In the daemon it goes to the
/// world's thread; in a worker to a thread that sends it to the daemon. Neither waits.
pub(crate) fn note(event: Event) {
    if let Some(world) = WORLD.get() {
        world.send(event);
        return;
    }
    if off() {
        return;
    }
    let sender = SENDER.get_or_init(|| {
        let (sender, events) = mpsc::channel::<Event>();
        let sending = move || {
            while let Ok(first) = events.recv() {
                send(std::iter::once(first).chain(events.try_iter()).collect());
            }
        };
        if std::thread::Builder::new().name("blaze-monitor-note".to_owned()).spawn(sending).is_err()
        {
            eprintln!("monitor: no thread to send events from");
        }
        sender
    });
    _ = sender.send(event);
}

/// Tells the monitor `event` from a client, whose build is over: one connection and one
/// write to the daemon's socket, which the kernel takes without waiting for the daemon. A
/// thread to send it from would have to be waited for at exit, which takes longer.
pub(crate) fn note_now(event: Event) {
    if !off() {
        send(vec![event]);
    }
}

/// Sends `events` to the daemon, if one listens, without waiting for an answer: there is
/// none. A daemon that is not there is not started for them.
fn send(events: Vec<Event>) {
    static VERSION: OnceLock<String> = OnceLock::new();
    let Ok(home) = home() else { return };
    let Ok(mut stream) = crate::socket::connect(&socket_in(&home)) else { return };
    let version = VERSION.get_or_init(version).clone();
    let envelope = Envelope { version, replaces: false, request: Request::Note(events) };
    let Ok(mut line) = serde_json::to_vec(&envelope) else { return };
    line.push(b'\n');
    _ = std::io::Write::write_all(&mut stream, &line);
}

/// Microseconds since the Unix epoch.
pub(crate) fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |it| it.as_micros() as u64)
}

/// The path as text, for an event.
pub(crate) fn shown(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A build a client ran, told when it ends (or when it is dropped, on an early return).
pub(crate) struct Build {
    event: event::Build,
    started: Instant,
    told: bool,
}

impl Build {
    pub(crate) fn begin(command: &str, workspace: &Path) -> Build {
        Build {
            event: event::Build {
                workspace: shown(workspace),
                command: command.to_owned(),
                by: event::By::Daemon,
                why: String::new(),
                succeeded: false,
                pid: std::process::id(),
                at_us: now_us(),
                took_us: 0,
            },
            started: Instant::now(),
            told: false,
        }
    }

    /// Who built it, and why when it was not the daemon.
    pub(crate) fn by(&mut self, by: event::By, why: &str) {
        self.event.by = by;
        self.event.why = why.lines().next().unwrap_or_default().to_owned();
    }

    /// The build is over and it succeeded: what runs next is not part of it.
    pub(crate) fn succeeded(mut self) {
        self.event.succeeded = true;
        self.tell();
    }

    fn tell(&mut self) {
        if !self.told {
            self.told = true;
            self.event.took_us = self.started.elapsed().as_micros() as u64;
            let event = Event::Build(self.event.clone());
            // The daemon notes the editor's edits it relays; a client sends its own builds.
            match WORLD.get() {
                Some(world) => world.send(event),
                None => note_now(event),
            }
        }
    }
}

impl Drop for Build {
    fn drop(&mut self) {
        self.tell();
    }
}

/// The executable process `pid` runs, where the system says.
pub(crate) fn executable_of(pid: u32) -> String {
    std::fs::read_link(format!("/proc/{pid}/exe")).map(|it| shown(&it)).unwrap_or_default()
}
