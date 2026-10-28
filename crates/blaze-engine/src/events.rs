//! What a worker or checker of the daemon says of the build it does, as it does it: the
//! records of [`blaze_protocol::events`], each a frame appended to a file beside the
//! process's socket and named for the process, so that one started in its place writes
//! another. The daemon follows the file while a command waits for the process ([`Follow`])
//! and hands each record on; a load, which comes before the process listens, is in it from
//! its first step. A record's time here is since the UNIX epoch: the daemon counts it from
//! when the command asked.

use std::{
    fs::{File, OpenOptions},
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

pub(crate) use blaze_protocol::events::Step;
use blaze_protocol::events::{Cause, CrateState, Event, Fns, Record};
use rustc_hash::FxHashMap;

/// The file this process says what it does in, once it has a socket.
static OUT: Mutex<Option<File>> = Mutex::new(None);

/// The file the process `pid` that listens at `socket` says what it does in.
pub(crate) fn beside(socket: &Path, pid: u32) -> PathBuf {
    socket.with_file_name(format!("events.{pid}"))
}

/// Takes away what the processes that listened at `socket` before said.
pub(crate) fn forget_beside(socket: &Path) {
    let Some(dir) = socket.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("events.") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The file the process that listens in `dir` says what it does in: the one written last,
/// of a process that started in the place of another that has not quite gone.
pub(crate) fn written_in(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let written = entries.flatten().filter(|it| it.file_name().to_string_lossy().starts_with("events."));
    let written = written.filter_map(|it| Some((it.metadata().ok()?.modified().ok()?, it.path())));
    written.max().map(|(_, path)| path)
}

/// Has this process say what it does beside `socket` from now on.
pub(crate) fn said_beside(socket: &Path) {
    let path = beside(socket, std::process::id());
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    if let Ok(file) = crate::socket::private_file(&mut options, &path) {
        *crate::lock(&OUT) = Some(file);
    }
}

/// Nanoseconds since the UNIX epoch.
pub(crate) fn now_ns() -> u64 {
    let since = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
}

/// Says `event`. A process with no socket says nothing.
pub(crate) fn say(event: Event) {
    let mut out = crate::lock(&OUT);
    let Some(file) = out.as_mut() else { return };
    let record = Record { at_ns: now_ns(), event };
    // One write of the whole frame: the daemon reads the file as it grows.
    if let Ok(item) = blaze_protocol::cbor::to_vec(&record) {
        let _ = blaze_protocol::write_frame(file, &item);
    }
}

/// The step the process is at, which the next one ends.
static AT: Mutex<Option<(Step, Option<String>)>> = Mutex::new(None);

/// Says the process is at `step`, of `of` when it is of one crate or program, and done
/// with the one before.
pub(crate) fn at(step: Step, of: Option<&str>) {
    let of = of.map(str::to_owned);
    if let Some((step, of)) = crate::lock(&AT).replace((step, of.clone())) {
        say(Event::Ended { step, of });
    }
    say(Event::Started { step, of });
}

/// Says the process is done with its step: it waits to be asked.
pub(crate) fn idle() {
    if let Some((step, of)) = crate::lock(&AT).take() {
        say(Event::Ended { step, of });
    }
}

/// How often a compile says how many functions it has compiled.
const COUNTED: Duration = Duration::from_millis(100);

/// The compile of the functions of `krate`, from now until this is dropped: a
/// [`Step::Codegen`], and every [`COUNTED`] the functions compiled so far.
pub(crate) struct Compiling {
    counting: Option<(std::sync::Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
}

pub(crate) fn compiling(krate: &str) -> Compiling {
    at(Step::Codegen, Some(krate));
    if crate::lock(&OUT).is_none() {
        return Compiling { counting: None };
    }
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let counting = {
        let (done, name) = (done.clone(), krate.to_owned());
        let from = hir_ty_codegen::compiled_so_far();
        std::thread::spawn(move || {
            let mut said = 0;
            while !done.load(Ordering::Relaxed) {
                std::thread::park_timeout(COUNTED);
                let compiled = (hir_ty_codegen::compiled_so_far() - from) as u64;
                if compiled != said {
                    said = compiled;
                    let fns = Fns { total: 0, done: compiled, kept: 0 };
                    say(Event::Crate { name: name.clone(), state: CrateState::Running, fns });
                }
            }
        })
    };
    Compiling { counting: Some((done, counting)) }
}

impl Drop for Compiling {
    fn drop(&mut self) {
        if let Some((done, counting)) = self.counting.take() {
            done.store(true, Ordering::Relaxed);
            counting.thread().unpark();
            let _ = counting.join();
        }
    }
}

/// Says what the build of `krate` came to, by the codegen's count, and of a build of an
/// edit, of the files `edited`, which functions it compiled again and why.
pub(crate) fn built(
    db: &dyn hir::db::HirDatabase,
    krate: &str,
    stats: &hir_ty_codegen::BuildStats,
    edited: Option<&FxHashMap<vfs::FileId, PathBuf>>,
) {
    let (recompiled, all) = hir_ty_codegen::take_recompiled(db);
    if let Some(edited) = edited {
        let count = (all - recompiled.len()) as u64;
        for it in recompiled {
            let cause = match edited.get(&it.file) {
                Some(file) => Cause::Edited { file: file.clone() },
                None => Cause::Uses,
            };
            say(Event::Rebuilt { name: it.name, krate: it.krate, cause });
        }
        if count > 0 {
            say(Event::Unlisted { count });
        }
    }
    let (bodies, reused) = (stats.bodies as u64, stats.reused as u64);
    let fns = Fns { total: bodies, done: bodies, kept: reused };
    let state = if stats.compiled == 0 { CrateState::UpToDate } else { CrateState::Built };
    say(Event::Crate { name: krate.to_owned(), state, fns });
}

/// The records a process writes to a file, read as it writes them.
pub(crate) struct Follow {
    path: Option<PathBuf>,
    at: u64,
    /// Bytes read of a frame not written in full yet.
    partial: Vec<u8>,
}

impl Follow {
    pub(crate) fn new() -> Follow {
        Follow { path: None, at: 0, partial: Vec::new() }
    }

    /// The records written to `path` since the last read, from the end of what it had when
    /// it was first named: a process says what it did before the command asked too. A file
    /// other than the last is read from its start, as a process started in another's place.
    pub(crate) fn read(&mut self, path: &Path, from_end: bool) -> Vec<Record> {
        if self.path.as_deref() != Some(path) {
            let len = std::fs::metadata(path).map_or(0, |it| it.len());
            let first = self.path.is_none();
            self.path = Some(path.to_owned());
            self.at = if first && from_end { len } else { 0 };
            self.partial.clear();
        }
        let Ok(mut file) = File::open(path) else { return Vec::new() };
        if file.seek(SeekFrom::Start(self.at)).is_err() {
            return Vec::new();
        }
        let mut bytes = std::mem::take(&mut self.partial);
        let Ok(read) = file.read_to_end(&mut bytes) else { return Vec::new() };
        self.at += read as u64;
        let mut records = Vec::new();
        let mut rest = &bytes[..];
        while rest.len() >= 4 {
            let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            let Some(item) = rest.get(4..4 + len) else { break };
            match blaze_protocol::cbor::from_slice::<Record>(item) {
                Ok(record) => records.push(record),
                Err(why) => eprintln!("{}: a record is not read: {why}", path.display()),
            }
            rest = &rest[4 + len..];
        }
        self.partial = rest.to_vec();
        records
    }
}

#[cfg(test)]
mod tests {
    use blaze_protocol::events::{Event, Level, Step};

    #[test]
    fn records_are_followed_as_they_are_written() {
        let dir = std::env::temp_dir().join(format!("blaze-events-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.1");
        let frame = |event: Event| {
            let record = blaze_protocol::events::Record { at_ns: 5, event };
            let item = blaze_protocol::cbor::to_vec(&record).unwrap();
            let mut frame = Vec::new();
            blaze_protocol::write_frame(&mut frame, &item).unwrap();
            frame
        };
        let before = frame(Event::Said { level: Level::Note, text: "before".to_owned() });
        std::fs::write(&path, &before).unwrap();
        let mut follow = super::Follow::new();
        assert!(follow.read(&path, true).is_empty(), "what was said before is not read");
        let started = frame(Event::Started { step: Step::Codegen, of: None });
        let ended = frame(Event::Ended { step: Step::Codegen, of: None });
        let mut written = before.clone();
        written.extend(&started);
        written.extend(&ended[..3]);
        std::fs::write(&path, &written).unwrap();
        let read = follow.read(&path, true);
        assert_eq!(read.len(), 1, "a frame half written waits for the rest");
        written.extend(&ended[3..]);
        std::fs::write(&path, &written).unwrap();
        let read = follow.read(&path, true);
        assert!(matches!(read[..], [blaze_protocol::events::Record { event: Event::Ended { .. }, .. }]));
        // A process in the place of the last writes another file, read from its start.
        let other = dir.join("events.2");
        std::fs::write(&other, &before).unwrap();
        assert_eq!(follow.read(&other, true).len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
