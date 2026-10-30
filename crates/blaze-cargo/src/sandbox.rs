//! Running a program in a sandbox: `blaze sandbox SPEC -- PROGRAM ARGS...`. The program sees
//! the machine's file system read-only, the directories [`Spec::overlays`] names as they are
//! but with its writes kept to itself, those [`Spec::writable`] names written for real, empty
//! temporary directories of its own, the environment the spec gives and no other, and no
//! network but its own loopback. What it reached outside is written to [`Spec::record`] as it
//! ends: the writes and the network the sandbox refused it, and, when [`Spec::record_reads`],
//! every path it opened, looked at, listed or ran, for what it read to be part of what its
//! result is a function of. `blaze test` runs every test executable in one
//! ([docs/cargo.md]); build scripts and proc macros are to run in the same ([#147]).
//!
//! This file is what every platform shares: the spec, the command, the probe, the record's
//! format and the marker a test harness gives the name of the test a thread runs by. Each
//! platform's sandbox is a module of its own with one entry, `run(spec, program)`: on Linux
//! namespaces, overlayfs and seccomp's user notification ([`linux`]); on macOS Seatbelt and a
//! library every process of the sandbox loads, which keeps the overlays' writes and records
//! (`sandbox/macos.rs`).
//!
//! [docs/cargo.md]: https://github.com/indexable-inc/blaze/blob/main/docs/cargo.md
//! [#147]: https://github.com/indexable-inc/blaze/issues/147

use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
};

use blaze_usercode::trace::{self, Seen};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(windows)]
pub mod windows;

pub mod v2;

/// The subcommand of `blaze` that runs a program in a sandbox.
pub const COMMAND: &str = "sandbox";

/// What a test harness looks up, on the thread of a test, just before the test with the
/// test's name after it and just after it with nothing after it (`access(MARK + name, F_OK)`):
/// the sandbox takes the calls of that thread in between to be the test's. Never a path of
/// the record. Outside a sandbox it is one failed `access` a test.
pub const MARK: &str = "/.blaze-test/";

/// What the interposer a traced run's processes load (`sandbox/linux_interpose.c` on Linux)
/// looks up to tell the supervisor what a seccomp filter can't see, each one failed
/// `access(path, F_OK)`, never a path of the record: a variable read by name after it
/// ([`ENV_MARK`]), a realtime clock read ([`CLOCK_MARK`]), and, as it loads, that it is in the
/// process ([`LOADED_MARK`]). Randomness the filter sees itself (`getrandom`, an open of
/// `/dev/random` or `/dev/urandom`).
pub const ENV_MARK: &str = "/.blaze-env/";
pub const CLOCK_MARK: &str = "/.blaze-clock";
pub const LOADED_MARK: &str = "/.blaze-loaded";

/// The status a sandbox that could not be made ends with, as `env` ends when it can't run
/// what it is given.
pub const COULD_NOT: i32 = 125;

/// The variable that turns sandboxes off (`off` or `0`), for tests, build scripts and
/// proc-macro servers alike.
pub const VAR: &str = "BLAZE_SANDBOX";

/// Whether sandboxes are on, with `var` looking a variable up: unless [`VAR`] is `off` or
/// `0`. The one rule tests, build scripts and proc-macro servers decide by.
pub fn on_with(var: impl Fn(&str) -> Option<String>) -> bool {
    let value = var(VAR);
    !matches!(value.as_deref(), Some("off" | "0"))
}

/// Whether sandboxes are on for this process, as [`on_with`] says of its environment.
pub fn on() -> bool {
    on_with(|name| std::env::var(name).ok())
}

/// What a program is run with.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Spec {
    /// The directory it starts in.
    pub cwd: PathBuf,
    /// All of its environment. A sandbox may add variables of its own, which no identity is
    /// made of.
    pub env: Vec<(String, String)>,
    /// Directories it sees as they are and may change, its changes seen by it alone and gone
    /// when it ends.
    pub overlays: Vec<PathBuf>,
    /// Directories it writes for real.
    #[serde(default)]
    pub writable: Vec<PathBuf>,
    /// Directories it sees as they are and may not write, though they are below one of the
    /// sandbox's empty directories (a workspace or target directory under `/tmp`): the rest of
    /// the machine is seen read-only anyway.
    #[serde(default)]
    pub readable: Vec<PathBuf>,
    /// The file written as it ends with what it reached outside ([`Line`]), or nothing.
    pub record: Option<PathBuf>,
    /// Whether the record has the paths it read, which costs: every call that names a path
    /// goes through the sandbox. What was refused it is recorded either way.
    #[serde(default)]
    pub record_reads: bool,
    /// Paths that are not inputs of what runs: the program itself, whose code is known.
    pub not_inputs: Vec<PathBuf>,
    /// The file system it sees in place of the machine's, when it is given one: then it sees
    /// nothing of the machine's but what the view binds, and the overlays, the writable and
    /// the readable directories are empty (what it sees is the view's binds alone).
    #[serde(default)]
    pub view: Option<View>,
}

/// What `blaze sandbox --v2` asks of a sandbox beyond a [`Spec`] ([`v2`]); nothing more for
/// the form `blaze sandbox SPEC -- PROGRAM`.
#[derive(Default, Clone, Copy)]
pub struct Also<'a> {
    /// Whether it may reach the network: what it connects to is a `T network` line.
    pub network: bool,
    /// Where each line goes as it is first seen, before the call it notes goes on; with one,
    /// the processes load the interposer, which sees what a sandbox's filter can't.
    pub live: Option<&'a Path>,
}

/// A file system of a program's own, as Nix gives a build: what it binds, each where the
/// program sees it, on a root of the sandbox's own that has nothing else but
///
/// - `/proc`, the PID namespace's own, which shows the sandbox's processes alone;
/// - `/dev` with `null`, `zero`, `full`, `random`, `urandom` and `tty`, the machine's,
///   `fd`, `stdin`, `stdout` and `stderr` linked into `/proc/self/fd`, and an empty `shm`;
/// - an empty `/tmp`;
/// - `/etc` with `passwd` and `group`, one line for the user it runs as and one for
///   `nobody`, whom files of other users show as, and `hosts`, `localhost` alone: what
///   Nix's sandbox gives, enough for `getpwuid` and a loopback name and nothing of the
///   machine's (no `resolv.conf`, there being no network to resolve on; no `nsswitch.conf`,
///   glibc reading the files without one).
///
/// The root, `/dev` and `/etc` are read-only. Everything else is not there: `/usr`, `$HOME`,
/// the store paths that are not bound (a `/nix/store` has those bound alone), the machine's
/// sockets. A path the program or anything it runs names outside the binds fails with
/// `ENOENT`, as the paths outside a Nix build's closure do. On Linux the view is a mount
/// namespace whose root replaces the machine's (`pivot_root`), the machine's tree detached
/// from it. On macOS a view is not made (decided): the program sees the machine as it would
/// without one.
///
/// A view for a build script, with a pinned compiler unpacked away from the store:
///
/// ```ignore
/// let view = View {
///     binds: vec![
///         Bind::read("/nix/store/abc-gcc-14", "/home/me/.blaze/nix/abc-gcc-14"),
///         Bind::read("/src/pkg", "/src/pkg"),
///         Bind::write("/out", "/target/debug/build/pkg-1/out"),
///     ],
/// };
/// ```
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// What it sees, in any order: one inside another is bound after it. Two never bind the
    /// same path, and none is `/` or below `/proc` or `/dev`, which are the view's own.
    pub binds: Vec<Bind>,
}

/// A directory or a file of the machine's, seen in a [`View`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    /// Where the program sees it: a whole path, with no `..`. Inside another bind it is a
    /// path that is there already, of the same kind (nothing is made in what is bound).
    pub inside: PathBuf,
    /// What it is on the machine, with the links on its way followed. One that is not there
    /// is an error, before anything runs.
    pub outside: PathBuf,
    /// Whether the program writes it for real; otherwise it is read-only, with what is below
    /// it.
    #[serde(default)]
    pub writable: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Bind {
    /// `outside`, seen read-only at `inside`.
    pub fn read(inside: impl Into<PathBuf>, outside: impl Into<PathBuf>) -> Bind {
        Bind { inside: inside.into(), outside: outside.into(), writable: false }
    }

    /// `outside`, written for real at `inside`.
    pub fn write(inside: impl Into<PathBuf>, outside: impl Into<PathBuf>) -> Bind {
        Bind { inside: inside.into(), outside: outside.into(), writable: true }
    }
}

/// What a line of a record says was reached.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Reached {
    /// `R`: a path opened, listed or run.
    Read,
    /// `L`: a path only looked at (`stat`, `access`, `chdir`, `readlink`): what it is, whether
    /// it can be run, a link's text and a file's size, but not what is in it.
    Looked,
    /// `W`: a write the sandbox refused: outside the overlays, the writable directories and
    /// the program's own empty ones.
    Write,
    /// `N`: an address the sandbox refused a connection, a bind or a message to.
    Net,
    /// `X`: the record is not whole, and why: what was read is not all known.
    Incomplete,
}

impl Reached {
    fn letter(self) -> char {
        match self {
            Reached::Read => 'R',
            Reached::Looked => 'L',
            Reached::Write => 'W',
            Reached::Net => 'N',
            Reached::Incomplete => 'X',
        }
    }

    fn of(letter: &str) -> Option<Reached> {
        Some(match letter {
            "R" => Reached::Read,
            "L" => Reached::Looked,
            "W" => Reached::Write,
            "N" => Reached::Net,
            "X" => Reached::Incomplete,
            _ => return None,
        })
    }

    /// What the record of the form `blaze sandbox SPEC -- PROGRAM` has for what a sandbox saw,
    /// which is a [`trace::Line`]: a listing is a read there, and variables and what can't be
    /// traced are not in it (that form loads no interposer and keys by none of them).
    fn of_seen(seen: Seen) -> Option<Reached> {
        Some(match seen {
            Seen::Read | Seen::Listed => Reached::Read,
            Seen::Looked => Reached::Looked,
            Seen::RefusedWrite => Reached::Write,
            Seen::RefusedNet => Reached::Net,
            Seen::Incomplete => Reached::Incomplete,
            Seen::Env | Seen::Untraceable => return None,
        })
    }
}

/// A line of the record `blaze sandbox SPEC -- PROGRAM` writes ([`Spec::record`]), made from
/// the same [`trace::Line`]s the live record of `--v2` has: `R`, `L`, `W`, `N` or `X`, the test, and the path, the address or why, by
/// tabs. The test is libtest's name of the one whose thread made the call, as [`MARK`] said,
/// or empty when that is not known (another thread, a child process, the program before or
/// after its tests): what has an empty test is taken to be every test's of the run. A record
/// has each line once, in order.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Line {
    pub reached: Reached,
    pub test: String,
    pub what: String,
}

/// The lines of the record `file`, `None` when there is none or it is not a record.
pub fn read_record(file: &Path) -> Option<Vec<Line>> {
    let text = std::fs::read_to_string(file).ok()?;
    text.lines()
        .map(|line| {
            let mut parts = line.splitn(3, '\t');
            let reached = Reached::of(parts.next()?)?;
            let (test, what) = (parts.next()?.to_owned(), parts.next()?.to_owned());
            Some(Line { reached, test, what })
        })
        .collect()
}

/// What a sandbox notes as its program runs: each [`trace::Line`] once, appended to the live
/// record as it is first seen when there is one ([`Recorder::live`]), and written as the record
/// of `blaze sandbox SPEC -- PROGRAM` when the run ends ([`Recorder::write`]).
pub struct Recorder {
    lines: BTreeSet<trace::Line>,
    /// The live record, each new line appended before the call it notes goes on.
    live: Option<std::fs::File>,
    /// Why a line could not be appended to the live record: it is not whole after that.
    live_failed: Option<String>,
    /// The scope each thread runs in, by the thread's id, as [`MARK`] said or as the thread
    /// that forked its process had it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    threads: HashMap<u64, String>,
    /// Every thread seen, scoped or not: one not seen yet is new.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    known: std::collections::HashSet<u64>,
    /// Whether the processes load the interposer: one that ran a program and never said it
    /// loaded is an `X` line ([`Recorder::ended`]).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    interposed: bool,
    /// The processes that ran a program and have not said the interposer loaded since, by
    /// process id: the program and the scope it ran in.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pending: std::collections::BTreeMap<u64, (String, String)>,
    reads: bool,
    not_inputs: BTreeSet<String>,
    /// Where writes are the program's: the overlays, the writable directories and the
    /// sandbox's own.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    written: Vec<String>,
    /// The readable directories: below one of the sandbox's own, but not written there.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    readable: Vec<String>,
    overlays: Vec<String>,
    /// The sandbox's own directories, whose paths are no inputs: the kernel's views and the
    /// empty directories.
    own: Vec<String>,
}

impl Recorder {
    /// For `spec`, in a sandbox whose own directories are `own`.
    pub fn new(spec: &Spec, own: &[&str]) -> Recorder {
        let tidied = |it: &Vec<PathBuf>| it.iter().map(|it| tidy(it)).collect::<Vec<_>>();
        let overlays = tidied(&spec.overlays);
        let mut written = overlays.clone();
        written.extend(tidied(&spec.writable));
        written.extend(own.iter().map(|it| (*it).to_owned()));
        let mut readable = tidied(&spec.readable);
        // A view's binds are where it writes and what it reads, as the writable and readable
        // directories are, whichever of the sandbox's own directories they are below.
        for bind in spec.view.iter().flat_map(|it| &it.binds) {
            match bind.writable {
                true => written.push(tidy(&bind.inside)),
                false => readable.push(tidy(&bind.inside)),
            }
        }
        // What is read there is an input, as what is read in an overlay is.
        let mut overlays = overlays;
        overlays.extend(readable.iter().cloned());
        Recorder {
            lines: BTreeSet::new(),
            live: None,
            live_failed: None,
            threads: HashMap::new(),
            known: std::collections::HashSet::new(),
            interposed: false,
            pending: std::collections::BTreeMap::new(),
            reads: spec.record_reads,
            not_inputs: spec.not_inputs.iter().map(|it| tidy(it)).collect(),
            written,
            readable,
            overlays,
            own: own.iter().map(|it| (*it).to_owned()).collect(),
        }
    }

    /// Appends each line to `file` as it is first seen, from now on.
    pub fn live(&mut self, file: &Path) -> Result<(), String> {
        let opened = std::fs::OpenOptions::new().append(true).open(file);
        self.live = Some(opened.map_err(|e| format!("opening the live record {}: {e}", file.display()))?);
        Ok(())
    }

    /// Takes the programs the run's processes run to load the interposer, which says so as it
    /// loads ([`LOADED_MARK`]): one that does not is noted at the end of the run.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn interposed(&mut self) {
        self.interposed = true;
    }

    /// `scope` (empty when it is not known) saw `what`, as `seen` says: for a sandbox that
    /// knows the scope of a call itself, not by its thread. A read, a listing, a variable and
    /// what can't be traced are kept only when reads are recorded, a path only when it is an
    /// input, as [`Recorder::read`] keeps it.
    pub fn reached(&mut self, seen: Seen, scope: String, what: &str) {
        let kept = match seen {
            Seen::Read | Seen::Looked | Seen::Listed => self.reads && self.input(what),
            Seen::Env | Seen::Untraceable => self.reads,
            Seen::RefusedWrite | Seen::RefusedNet | Seen::Incomplete => true,
        };
        if !kept {
            return;
        }
        let line = trace::Line::new(seen, &scope, what);
        if self.lines.contains(&line) {
            return;
        }
        if let Some(file) = &mut self.live
            && self.live_failed.is_none()
            && let Err(e) = file.write_all(line.text().as_bytes())
        {
            self.live_failed = Some(e.to_string());
        }
        self.lines.insert(line);
    }

    /// Every line seen, each once, in order.
    pub fn lines(&self) -> impl Iterator<Item = &trace::Line> {
        self.lines.iter()
    }

    /// Why the live record is not whole, when a line could not be appended to it.
    pub fn live_failed(&self) -> Option<&str> {
        self.live_failed.as_deref()
    }

    /// The run ended: each process that ran a program which never said the interposer loaded
    /// (a static or setuid one, or one that cleared the variables that load it) read what was
    /// not traced. A program that failed to start is one too: it read nothing, but is not told
    /// from one that did.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn ended(&mut self) {
        for (program, scope) in std::mem::take(&mut self.pending).into_values() {
            let why = format!("{program} ran without the interposer, so what it read is not known");
            self.reached(Seen::Incomplete, scope, &why);
        }
    }

    /// Whether `path`, made whole, is an input: not one of `not_inputs`, and not the sandbox's
    /// own but where an overlay is.
    fn input(&self, path: &str) -> bool {
        !self.not_inputs.contains(path)
            && (self.overlays.iter().any(|it| under(path, it))
                || !self.own.iter().any(|it| under(path, it)))
    }

    /// Writes the record of `blaze sandbox SPEC -- PROGRAM` to `file`, whole or not at all.
    pub fn write(&self, file: &Path) {
        let lines: BTreeSet<Line> = self
            .lines
            .iter()
            .filter_map(|it| {
                let reached = Reached::of_seen(it.seen)?;
                Some(Line { reached, test: it.scope.clone(), what: it.what.clone() })
            })
            .collect();
        let mut text = String::new();
        for line in &lines {
            text.push(line.reached.letter());
            text.push('\t');
            text.push_str(&line.test);
            text.push('\t');
            text.push_str(&line.what);
            text.push('\n');
        }
        let writing = file.with_extension("writing");
        if std::fs::write(&writing, text).and_then(|()| std::fs::rename(&writing, file)).is_err() {
            _ = std::fs::remove_file(file);
        }
    }
}

/// What a sandbox that knows the test of a call by its thread notes: the Linux one, whose
/// supervisor sees each call's thread and the harness's marker.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Recorder {
    /// Whether the reads are recorded.
    pub fn reads(&self) -> bool {
        self.reads
    }

    /// Takes `path`, as `thread` of `process` named it, as one of the markers when it is:
    /// [`MARK`] (the thread runs in the scope after it, or none), and, when the processes load
    /// the interposer, [`ENV_MARK`], [`CLOCK_MARK`] and [`LOADED_MARK`]. Whether it was one.
    pub fn marked(&mut self, thread: u64, process: u64, path: &[u8]) -> bool {
        self.known.insert(thread);
        if let Some(name) = path.strip_prefix(MARK.as_bytes()) {
            match name.is_empty() {
                true => _ = self.threads.remove(&thread),
                false => _ = self.threads.insert(thread, String::from_utf8_lossy(name).into_owned()),
            }
            return true;
        }
        if !self.interposed {
            return false;
        }
        if let Some(name) = path.strip_prefix(ENV_MARK.as_bytes()) {
            // A name that is not UTF-8 is read as one that is: the variable is an input
            // either way, and `Traced::env` names variables as text.
            self.reached(Seen::Env, self.scope(thread), &String::from_utf8_lossy(name));
            return true;
        }
        if path == CLOCK_MARK.as_bytes() {
            self.untraceable(thread, blaze_core::Untraceable::Clock);
            return true;
        }
        if path == LOADED_MARK.as_bytes() {
            self.pending.remove(&process);
            return true;
        }
        false
    }

    /// The scope `thread` runs in, empty when none.
    pub fn scope(&self, thread: u64) -> String {
        self.threads.get(&thread).cloned().unwrap_or_default()
    }

    /// Whether `thread` was seen before.
    pub fn knows(&self, thread: u64) -> bool {
        self.known.contains(&thread)
    }

    /// `thread`, not seen before, starts in `scope` (empty for none): a new process starts in
    /// the scope the thread that forked it had then.
    pub fn starts(&mut self, thread: u64, scope: String) {
        if self.known.insert(thread) && !scope.is_empty() {
            self.threads.insert(thread, scope);
        }
    }

    /// `thread` read `path`, made whole: recorded when reads are and it is an input.
    pub fn read(&mut self, thread: u64, path: &str) {
        self.reached(Seen::Read, self.scope(thread), path);
    }

    /// `thread` looked at `path`, made whole, without opening it: recorded as [`Self::read`]
    /// records a read.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn looked(&mut self, thread: u64, path: &str) {
        self.reached(Seen::Looked, self.scope(thread), path);
    }

    /// `thread` listed the directory `path`, made whole: recorded as [`Self::read`] records a
    /// read.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn listed(&mut self, thread: u64, path: &str) {
        self.reached(Seen::Listed, self.scope(thread), path);
    }

    /// `thread` got what can't be traced.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn untraceable(&mut self, thread: u64, it: blaze_core::Untraceable) {
        self.reached(Seen::Untraceable, self.scope(thread), trace::untraceable_name(it));
    }

    /// `thread` of `process` runs the program `path`: when the processes load the interposer,
    /// it is to say so before the run ends.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn runs(&mut self, thread: u64, process: u64, path: &str) {
        if self.interposed && self.reads {
            self.pending.insert(process, (path.to_owned(), self.scope(thread)));
        }
    }

    /// Whether a write to `path` is the program's to make.
    pub fn may_write(&self, path: &str) -> bool {
        self.written.iter().any(|it| under(path, it) && !self.only_read(path, it))
    }

    /// Whether `path` is in a readable directory that is below `dir`, one of the sandbox's
    /// own: it is seen there read-only.
    fn only_read(&self, path: &str, dir: &str) -> bool {
        self.readable.iter().any(|it| under(it, dir) && it != dir && under(path, it))
            && !self.written.iter().any(|it| under(it, dir) && it != dir && under(path, it))
    }

    /// Whether a connection to the Unix socket at `path`, made whole with its links followed,
    /// is the program's to make: one in the overlays, the writable directories or the
    /// sandbox's empty ones, which only the program's sockets can be in; not one of the
    /// kernel's views or the machine's `/dev`, which the sandbox sees as they are.
    pub fn may_connect(&self, path: &str) -> bool {
        let seen_as_is = ["/proc", "/sys", "/dev"].iter().any(|it| under(path, it));
        self.may_write(path) && (!seen_as_is || under(path, "/dev/shm"))
    }

    /// The sandbox refused `thread` a write to `path`.
    pub fn refused_write(&mut self, thread: u64, path: &str) {
        self.reached(Seen::RefusedWrite, self.scope(thread), path);
    }

    /// The sandbox refused `thread` the network at `address`.
    pub fn refused_net(&mut self, thread: u64, address: &str) {
        self.reached(Seen::RefusedNet, self.scope(thread), address);
    }

    /// The record is not whole for what `thread` did, and why.
    pub fn incomplete(&mut self, thread: u64, why: &str) {
        self.reached(Seen::Incomplete, self.scope(thread), why);
    }
}

/// The process group the signals the sandbox's own process is sent are passed on to: the
/// program's session, which the terminal no longer sends them to. 0 for none.
#[cfg(unix)]
static GROUP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn pass_on(signal: libc::c_int) {
    let group = GROUP.load(std::sync::atomic::Ordering::Relaxed);
    if group > 0 {
        // SAFETY: `kill` is async-signal-safe.
        unsafe { libc::kill(-group, signal) };
    }
}

/// Has the signals a terminal or the caller ends a run with (`SIGINT`, `SIGTERM`, `SIGHUP`,
/// `SIGQUIT`) passed on to the process group `group`, the program's session, rather than end
/// the sandbox's own process, which then ends as the program does; 0 once the group is gone.
/// The handlers are made the first time: a process that forks after keeps them until it runs
/// a program, and passes nothing on.
#[cfg(unix)]
pub fn pass_signals_to(group: libc::pid_t) {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    GROUP.store(group, std::sync::atomic::Ordering::Relaxed);
    INSTALLED.call_once(|| {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            // SAFETY: `pass_on` only loads an atomic and calls `kill`.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = pass_on as extern "C" fn(libc::c_int) as usize;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    });
}

/// Whether `path` is `dir` or below it.
fn under(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `blaze sandbox SPEC -- PROGRAM ARGS...`, `blaze sandbox --v2 SPEC_FILE` ([`v2::command`]),
/// or `blaze sandbox --probe [--overlays]`: runs the program and ends as it ended. Runs before
/// anything else in the process, which is one thread: it forks.
pub fn command(mut args: impl Iterator<Item = OsString>) -> ! {
    let first = args.next();
    if first.as_deref() == Some("--v2".as_ref()) {
        v2::command(args)
    }
    if first.as_deref() == Some("--probe".as_ref()) {
        let kind = match args.next() {
            None => Probe::Writable,
            Some(it) if it == "--overlays" => Probe::Overlays,
            Some(it) if it == "--v2" => Probe::V2,
            Some(it) => {
                eprintln!("blaze sandbox: --probe takes --overlays or --v2, not {}", it.display());
                std::process::exit(COULD_NOT);
            }
        };
        std::process::exit(match probe(kind) {
            Ok(()) => 0,
            Err(why) => {
                eprintln!("{why}");
                1
            }
        });
    }
    let spec = first.ok_or("a spec").and_then(|it| {
        let text = std::fs::read_to_string(&it).map_err(|_| "a spec it can read")?;
        serde_json::from_str::<Spec>(&text).map_err(|_| "a spec it can parse")
    });
    let program: Vec<OsString> = args.skip_while(|it| it != "--").skip(1).collect();
    let spec = match (spec, program.is_empty()) {
        (Ok(spec), false) => spec,
        (Err(what), _) => {
            eprintln!("blaze sandbox: needs {what}: blaze sandbox SPEC -- PROGRAM ARGS...");
            std::process::exit(COULD_NOT);
        }
        (_, true) => {
            eprintln!("blaze sandbox: needs a program: blaze sandbox SPEC -- PROGRAM ARGS...");
            std::process::exit(COULD_NOT);
        }
    };
    std::process::exit(match backend(&spec, Some(&program), Also::default()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("blaze: the sandbox could not be made: {error}");
            COULD_NOT
        }
    })
}

/// Runs `program` in the sandbox `spec` and `also` say, or with `None` makes the sandbox and
/// runs nothing: what it ends with, its status or 128 and the signal that ended it.
fn backend(spec: &Spec, program: Option<&[OsString]>, also: Also<'_>) -> Result<i32, String> {
    #[cfg(target_os = "linux")]
    return linux::run(spec, program, also);
    #[cfg(target_os = "macos")]
    return macos::run(spec, program, also);
    #[cfg(windows)]
    return windows_backend(spec, program, also);
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        _ = (spec, program, also);
        Err(format!("there is no sandbox on {} yet", std::env::consts::OS))
    }
}

/// [`backend`] on Windows, by [`windows::run`]: an AppContainer has no overlays and no views,
/// and traces nothing, so a record is one `X` line.
#[cfg(windows)]
fn windows_backend(spec: &Spec, program: Option<&[OsString]>, also: Also<'_>) -> Result<i32, String> {
    if !spec.overlays.is_empty() {
        return Err("there are no overlays on Windows: a test's sandbox can't be made".to_owned());
    }
    if spec.view.is_some() {
        return Err("there are no views of the machine on Windows".to_owned());
    }
    let Some((name, args)) = program.and_then(|it| it.split_first()) else {
        return windows::probe().map(|()| 0);
    };
    let confined = blaze_usercode::confine::Spec {
        program: PathBuf::from(name),
        args: args.to_vec(),
        env: spec.env.iter().map(|(k, v)| (k.into(), v.into())).collect(),
        dir: spec.cwd.clone(),
        read: spec.readable.clone(),
        write: spec.writable.clone(),
        network: also.network,
        trace: false,
        live: PathBuf::new(),
        report: PathBuf::new(),
        not_inputs: Vec::new(),
    };
    let code = windows::run(&confined)?;
    if let Some(file) = &spec.record {
        let mut recorder = Recorder::new(spec, &[]);
        recorder.reached(Seen::Incomplete, String::new(), v2::NOT_TRACED_ON_WINDOWS);
        recorder.write(file);
    }
    Ok(code)
}

/// Whether programs can be run in a sandbox here, with `overlays` (as tests are) or without
/// (as build scripts are), and why not when they can't: the probe makes one around nothing, in
/// a process of its own, once per process that asks.
pub fn available(overlays: bool) -> Result<(), String> {
    probed(match overlays {
        true => Probe::Overlays,
        false => Probe::Writable,
    })
}

/// What a probe makes a sandbox for.
#[derive(Clone, Copy)]
enum Probe {
    /// A writable directory, as a build script's sandbox has.
    Writable,
    /// An overlay, as a test's sandbox has.
    Overlays,
    /// What `blaze sandbox --v2` makes: a writable directory and a live record, and on Linux
    /// the interposer the run's processes load.
    V2,
}

/// [`available`] for `kind`, the probe run once per process.
fn probed(kind: Probe) -> Result<(), String> {
    static AVAILABLE: [std::sync::OnceLock<Result<(), String>>; 3] =
        [std::sync::OnceLock::new(), std::sync::OnceLock::new(), std::sync::OnceLock::new()];
    AVAILABLE[kind as usize]
        .get_or_init(|| {
            if !cfg!(any(target_os = "linux", target_os = "macos", windows)) {
                return Err(format!("there is no sandbox on {} yet", std::env::consts::OS));
            }
            let exe = std::env::current_exe().map_err(|it| format!("blaze is not found ({it})"))?;
            let flag = match kind {
                Probe::Writable => None,
                Probe::Overlays => Some("--overlays"),
                Probe::V2 => Some("--v2"),
            };
            let out = std::process::Command::new(exe)
                .args([COMMAND, "--probe"])
                .args(flag)
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|it| format!("the probe did not run ({it})"))?;
            match out.status.success() {
                true => Ok(()),
                false => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
            }
        })
        .clone()
}

/// Makes a sandbox around nothing, as `kind` says: `Ok` when it could be made, why not
/// otherwise, with what would let it be where that is known.
fn probe(kind: Probe) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("blaze-sandbox-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
    let spec = match kind {
        Probe::Overlays => Spec { cwd: dir.clone(), overlays: vec![dir.clone()], ..Spec::default() },
        Probe::Writable | Probe::V2 => {
            Spec { cwd: dir.clone(), writable: vec![dir.clone()], ..Spec::default() }
        }
    };
    let live = dir.join("live");
    let also = match kind {
        Probe::V2 => {
            std::fs::write(&live, "").map_err(|it| format!("{}: {it}", live.display()))?;
            Also { network: false, live: Some(&live) }
        }
        Probe::Writable | Probe::Overlays => Also::default(),
    };
    let ended = backend(&spec, None, also);
    _ = std::fs::remove_dir_all(&dir);
    let restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns");
    let hint = match restricted.is_ok_and(|it| it.trim() == "1") {
        true => {
            " (AppArmor keeps users from making namespaces; \
             `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0` lets them, \
             or an AppArmor profile that allows `userns` for blaze)"
        }
        false => "",
    };
    match ended {
        Ok(0) => Ok(()),
        Ok(_) => Err(format!("the sandbox could not be made (it said why above){hint}")),
        Err(error) => Err(format!("{error}{hint}")),
    }
}

/// What a container this process is in is known by, below `root` (`/` but in tests), with
/// `container` the variable of that name: `/.dockerenv` (Docker), `/run/.containerenv` (Podman),
/// the variable (systemd-nspawn, Podman, LXC), or `/proc/1/cgroup` naming a runtime (cgroup v1
/// Docker, Kubernetes, containerd). `None` outside one, as far as it can be told.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn container(root: &Path, container: Option<&str>) -> Option<String> {
    for file in [".dockerenv", "run/.containerenv"] {
        if root.join(file).exists() {
            return Some(format!("/{file}"));
        }
    }
    if let Some(name) = container.filter(|it| !it.is_empty()) {
        return Some(format!("`container={name}`"));
    }
    let cgroup = std::fs::read_to_string(root.join("proc/1/cgroup")).unwrap_or_default();
    ["docker", "kubepods", "containerd", "libpod", "lxc"]
        .into_iter()
        .find(|it| cgroup.contains(it))
        .map(|it| format!("/proc/1/cgroup naming {it}"))
}

/// What is said after a user namespace refused with `EPERM` in a container known by `known`
/// ([`container`]): the container's seccomp profile (Docker's default) is what refuses it, and
/// the ways on: a profile that allows it, or no sandbox at all.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn container_hint(known: &str) -> String {
    format!(
        "; this runs in a container ({known}), and its seccomp profile (Docker's default) refuses \
         `unshare(CLONE_NEWUSER)`: run the container with `--security-opt seccomp=unconfined` \
         (or a seccomp profile that allows user namespaces; on Kubernetes \
         `securityContext.seccompProfile.type: Unconfined`), or set `{VAR}=off` to run \
         build scripts, proc macros and tests outside a sandbox, as cargo does"
    )
}

/// `path` with `.` and empty parts left out.
fn tidy(path: &Path) -> String {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blaze_sandbox_off_or_0_turns_sandboxes_off() {
        let with = |vars: &'static [(&str, &str)]| {
            on_with(|name| vars.iter().find(|it| it.0 == name).map(|it| it.1.to_owned()))
        };
        assert!(with(&[]) && with(&[("BLAZE_SANDBOX", "on")]));
        assert!(!with(&[("BLAZE_SANDBOX", "off")]) && !with(&[("BLAZE_SANDBOX", "0")]));
    }

    // The paths a sandbox records are a Unix machine's: there is one on Linux and macOS alone.
    #[cfg(unix)]
    #[test]
    fn a_container_is_known_by_its_marks() {
        let root =
            std::env::temp_dir().join(format!("blaze-container-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc/1")).unwrap();
        std::fs::create_dir_all(root.join("run")).unwrap();
        assert_eq!(container(&root, None), None);
        assert_eq!(container(&root, Some("")), None);
        assert_eq!(container(&root, Some("podman")).as_deref(), Some("`container=podman`"));
        std::fs::write(root.join("proc/1/cgroup"), "0::/\n").unwrap();
        assert_eq!(container(&root, None), None);
        std::fs::write(root.join("proc/1/cgroup"), "12:pids:/kubepods/burstable/pod1/abc\n")
            .unwrap();
        assert_eq!(container(&root, None).as_deref(), Some("/proc/1/cgroup naming kubepods"));
        std::fs::write(root.join("run/.containerenv"), "").unwrap();
        assert_eq!(container(&root, None).as_deref(), Some("/run/.containerenv"));
        std::fs::write(root.join(".dockerenv"), "").unwrap();
        let known = container(&root, Some("podman")).unwrap();
        assert_eq!(known, "/.dockerenv");
        let hint = container_hint(&known);
        assert!(hint.contains("container (/.dockerenv)"), "{hint}");
        assert!(hint.contains("--security-opt seccomp=unconfined"), "{hint}");
        assert!(hint.contains("`BLAZE_SANDBOX=off`"), "{hint}");
        _ = std::fs::remove_dir_all(&root);
    }

    // The paths a sandbox records are a Unix machine's: there is one on Linux and macOS alone.
    #[cfg(unix)]
    #[test]
    fn a_record_is_its_lines_in_order_once() {
        let spec = Spec {
            overlays: vec!["/ws".into()],
            not_inputs: vec!["/ws/target/t".into(), "/bin/t".into()],
            record_reads: true,
            ..Spec::default()
        };
        let mut recorder = Recorder::new(&spec, &["/proc", "/tmp"]);
        recorder.read(1, "/etc/hosts");
        assert!(recorder.marked(7, 1, b"/.blaze-test/tests::a"));
        recorder.read(7, "/ws/data.txt");
        recorder.read(7, "/ws/data.txt");
        recorder.read(7, "/proc/self/maps");
        recorder.read(7, "/bin/t");
        recorder.refused_write(7, "/home/u/.config/x");
        recorder.refused_net(1, "1.2.3.4:443");
        assert!(recorder.marked(7, 1, b"/.blaze-test/"));
        recorder.read(7, "/etc/passwd");
        recorder.read(7, "/ws/a\tb");
        assert!(!recorder.marked(7, 1, b"/etc/x"));
        assert!(!recorder.marked(7, 1, b"/.blaze-env/HOME"), "no interposer, no variables");
        recorder.listed(7, "/ws/src");
        recorder.untraceable(7, blaze_core::Untraceable::Random);
        assert!(recorder.may_write("/ws/target/x") && recorder.may_write("/tmp"));
        assert!(!recorder.may_write("/wsx") && !recorder.may_write("/etc/x"));
        assert!(recorder.may_connect("/tmp/s.sock") && recorder.may_connect("/ws/target/s"));
        assert!(!recorder.may_connect("/proc/1/root/run/docker.sock"));
        assert!(!recorder.may_connect("/run/docker.sock") && !recorder.may_connect("/etc/s"));
        let dir = std::env::temp_dir().join(format!("blaze-record-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("record");
        recorder.write(&file);
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            text,
            "R\t\t/etc/hosts\nR\t\t/etc/passwd\nR\t\t/ws/src\nR\ttests::a\t/ws/data.txt\n\
             W\ttests::a\t/home/u/.config/x\nN\t\t1.2.3.4:443\n\
             X\t\ta path or a name with a tab or a line break in it\n"
        );
        let lines = read_record(&file).unwrap();
        assert_eq!(lines.len(), 7);
        assert_eq!(
            lines[3],
            Line { reached: Reached::Read, test: "tests::a".into(), what: "/ws/data.txt".into() }
        );
        _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_live_record_has_each_line_once_as_it_is_seen() {
        let dir = std::env::temp_dir().join(format!("blaze-live-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("live");
        std::fs::write(&live, "E\t\t*\n").unwrap();
        let spec = Spec { overlays: vec!["/ws".into()], record_reads: true, ..Spec::default() };
        let mut recorder = Recorder::new(&spec, &["/tmp"]);
        recorder.live(&live).unwrap();
        recorder.interposed();
        assert!(recorder.marked(7, 7, b"/.blaze-test/tests::a"));
        recorder.read(7, "/ws/a");
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "E\t\t*\nR\ttests::a\t/ws/a\n");
        recorder.read(7, "/ws/a");
        recorder.read(7, "/tmp/scratch");
        assert!(recorder.marked(7, 7, b"/.blaze-env/HOME"));
        assert!(recorder.marked(8, 7, b"/.blaze-clock"));
        recorder.listed(7, "/ws");
        recorder.untraceable(8, blaze_core::Untraceable::Network);
        let (lines, used) = trace::parse(&std::fs::read_to_string(&live).unwrap()).unwrap();
        assert_eq!(used, std::fs::read_to_string(&live).unwrap().len());
        let texts: Vec<String> = lines.iter().map(trace::Line::text).collect();
        assert_eq!(
            texts,
            [
                "E\t\t*\n",
                "R\ttests::a\t/ws/a\n",
                "E\ttests::a\tHOME\n",
                "T\t\tclock\n",
                "D\ttests::a\t/ws\n",
                "T\t\tnetwork\n",
            ]
        );
        assert_eq!(recorder.live_failed(), None);
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_program_that_never_says_it_loaded_the_interposer_is_not_traced() {
        let spec = Spec { record_reads: true, ..Spec::default() };
        let mut recorder = Recorder::new(&spec, &[]);
        recorder.interposed();
        assert!(recorder.marked(10, 10, b"/.blaze-test/tests::b"));
        recorder.runs(10, 10, "/bin/loads");
        recorder.runs(11, 11, "/bin/static");
        assert!(recorder.marked(10, 10, b"/.blaze-loaded"));
        recorder.ended();
        let lines: Vec<_> = recorder.lines().cloned().collect();
        assert_eq!(
            lines,
            [trace::Line::new(
                Seen::Incomplete,
                "",
                "/bin/static ran without the interposer, so what it read is not known",
            )]
        );
    }

    #[test]
    fn a_new_process_starts_in_the_scope_it_is_given_and_a_thread_once() {
        let mut recorder = Recorder::new(&Spec { record_reads: true, ..Spec::default() }, &[]);
        assert!(!recorder.knows(20));
        recorder.starts(20, "tests::c".to_owned());
        assert!(recorder.knows(20));
        assert_eq!(recorder.scope(20), "tests::c");
        recorder.starts(20, "tests::d".to_owned());
        assert_eq!(recorder.scope(20), "tests::c", "a thread seen before keeps its scope");
        assert!(recorder.marked(20, 20, b"/.blaze-test/"));
        assert_eq!(recorder.scope(20), "");
    }
}
