//! The runtime of a program blaze-link linked: it takes patches while it runs.
//!
//! Every function a patch can replace is called through a slot of a jump table: a call loads
//! the table cell, then the slot. A patch writes its code and data where the linker placed
//! them, then publishes a table with its slots changed in one atomic store to the cell, so
//! every thread's next call through any slot sees all of the patch or none of it. Going back
//! to a kept generation (the last [`KEPT`](switch::KEPT)) is one store again: when the linker
//! says so ([`crate::patch::Message::RollBack`]), or when code of a generation panics under
//! [`Live::guard`].
//!
//! The descriptor ([`crate::patch::Descriptor`]) says where everything is. A program at
//! generation `g` takes the message in the file [`message_file`]`(g)` of the directory it
//! names, read in place from a mapping of the file, and answers each there
//! ([`crate::patch::Answer`]). A thread blocks on the platform's notification of changes to
//! the directory (inotify, kqueue, change notifications) and counts them, so [`Live::poll`]
//! is one atomic load while nothing came.
//!
//! Linux on x86-64 and AArch64, macOS on AArch64 and Windows on x86-64; elsewhere
//! [`Live::start`] says so.

use std::{
    collections::VecDeque,
    fmt,
    fs::{File, Metadata},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::SystemTime,
};

use crate::patch::{ANSWER, Answer, DESCRIPTOR, DESCRIPTOR_MAGIC, Descriptor, Message, message_file};
use os::backtrace;
use switch::{KEPT, Layout, Switch};

mod hook;
mod switch;
#[cfg(test)]
mod tests;

#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
))]
mod unix;

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod linux;
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
use linux as os;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use macos as os;

#[cfg(all(windows, target_arch = "x86_64"))]
mod windows;
#[cfg(all(windows, target_arch = "x86_64"))]
use windows as os;

#[cfg(not(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
)))]
mod os {
    use std::{fs::File, path::Path, sync::Arc};

    use super::{
        Events,
        switch::{Dwarf, Layout, Memory, Writes},
    };

    const PLATFORMS: &str =
        "live patches run on Linux x86-64 and AArch64, macOS AArch64 and Windows x86-64";

    pub(super) enum Os {}

    impl Os {
        pub(super) fn new(_: &Layout) -> Result<Os, String> {
            Err(PLATFORMS.to_owned())
        }
    }

    impl Memory for Os {
        const DWARF: Dwarf = Dwarf::Not;
        const SEH: bool = false;

        fn code(&mut self, _: &Writes<'_>) -> Result<(), String> {
            match *self {}
        }

        fn data(&mut self, _: &Writes<'_>) -> Result<(), String> {
            match *self {}
        }

        fn frames(&mut self, _: &[u64]) -> Result<(), String> {
            match *self {}
        }

        fn functions(&mut self, _: &[(u64, u64, u64)]) -> Result<(), String> {
            match *self {}
        }

        fn publish(&mut self, _: u64, _: u64) -> Result<(), String> {
            match *self {}
        }
    }

    pub(super) enum Map {}

    impl Map {
        pub(super) fn new(_: &File, _: u64) -> Result<Map, String> {
            Err(PLATFORMS.to_owned())
        }

        pub(super) fn bytes(&self) -> &[u8] {
            match *self {}
        }
    }

    pub(super) fn descriptor() -> Result<u64, String> {
        Err(PLATFORMS.to_owned())
    }

    pub(super) fn watch(_: &Path, _: Arc<Events>) -> Result<(), String> {
        Err(PLATFORMS.to_owned())
    }

    pub(super) fn backtrace(_: &mut Vec<u64>) {}
}

/// What became of a message, or of a panic under [`Live::guard`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The program runs `generation` now.
    Applied { generation: u64, functions: u64, statics: u64 },
    /// The program left `generation` for `to`, for this reason: the linker asked, or a panic
    /// in its code (`panicked at <file>:<line>:<column>: <message>`).
    RolledBack { generation: u64, to: u64, why: String },
    /// The edit needs the program started again, for this reason.
    Restart { generation: u64, why: String },
    /// The message could not be taken, for this reason; the program runs on as it was.
    Declined { generation: u64, why: String },
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Applied { generation, functions, statics } => {
                write!(f, "generation {generation} applied: {functions} functions")?;
                if *statics != 0 {
                    write!(f, ", {statics} statics")?;
                }
                Ok(())
            }
            Outcome::RolledBack { generation, to, why } => {
                write!(f, "generation {generation} rolled back to {to}: {why}")
            }
            Outcome::Restart { generation, why } => {
                write!(f, "restart needed at generation {generation}: {why}")
            }
            Outcome::Declined { generation, why } => {
                write!(f, "generation {generation} not applied: {why}")
            }
        }
    }
}

/// Whether a [`Live`] was started in this process: the image has one table cell.
static STARTED: AtomicBool = AtomicBool::new(false);

/// What the thread watching the directory tells [`Live::poll`]: how many times a message
/// file may have come, and when to stop.
#[derive(Default)]
pub(crate) struct Events {
    count: AtomicU64,
    stop: AtomicBool,
}

impl Events {
    pub(crate) fn tell(&self) {
        self.count.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// Which file a message was read from: a file renamed over it is another.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Identity {
    len: u64,
    modified: Option<SystemTime>,
    /// The inode on Unix, the creation time on Windows.
    node: u64,
}

impl Identity {
    fn of(meta: &Metadata) -> Identity {
        #[cfg(unix)]
        let node = std::os::unix::fs::MetadataExt::ino(meta);
        #[cfg(windows)]
        let node = std::os::windows::fs::MetadataExt::creation_time(meta);
        #[cfg(not(any(unix, windows)))]
        let node = 0;
        Identity { len: meta.len(), modified: meta.modified().ok(), node }
    }
}

/// The answer file of the directory, and the bytes of the last answer, reused.
struct Answers {
    path: PathBuf,
    partial: PathBuf,
    bytes: Vec<u8>,
}

impl Answers {
    fn new(dir: &Path) -> Answers {
        Answers {
            path: dir.join(ANSWER),
            partial: dir.join(format!("{ANSWER}.{}", std::process::id())),
            bytes: Vec::new(),
        }
    }

    /// Writes `answer` in one write, then renames it over the last.
    fn write(&mut self, answer: &Answer) {
        self.bytes.clear();
        answer.encode_into(&mut self.bytes);
        let written = std::fs::write(&self.partial, &self.bytes)
            .and_then(|()| std::fs::rename(&self.partial, &self.path));
        if let Err(error) = written {
            eprintln!("blaze-live: answering in {}: {error}", self.path.display());
        }
    }
}

/// A program taking patches.
pub struct Live {
    switch: Switch<os::Os>,
    dir: PathBuf,
    answers: Answers,
    events: Arc<Events>,
    /// The count of events and the generation when the program last looked for its message.
    looked: Option<(u64, u64)>,
    /// The files of the last messages taken, by generation: none is taken twice.
    taken: VecDeque<(u64, Identity)>,
}

impl Live {
    /// Finds this image's descriptor, readies its patch code region for writes, installs the
    /// panic hook [`Live::guard`] needs and starts watching for messages.
    pub fn start() -> Result<Live, String> {
        if STARTED.swap(true, Ordering::AcqRel) {
            return Err("blaze-live's slots were started already in this program".to_owned());
        }
        let live = Live::open();
        if live.is_err() {
            STARTED.store(false, Ordering::Release);
        }
        live
    }

    fn open() -> Result<Live, String> {
        let at = os::descriptor()?;
        // SAFETY: the symbol is the descriptor the linker wrote, `Descriptor::SIZE` bytes of
        // the image's read-only data, mapped as long as the image is.
        let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, Descriptor::SIZE) };
        let descriptor = Descriptor::decode(bytes).ok_or_else(|| {
            format!(
                "{DESCRIPTOR} at {at:#x} does not start with {}",
                String::from_utf8_lossy(&DESCRIPTOR_MAGIC)
            )
        })?;
        let layout = Layout::new(&descriptor, at)?;
        let dir = directory(&layout)?;
        let memory = os::Os::new(&layout)?;
        let switch = Switch::new(memory, layout)?;
        let mut create = std::fs::DirBuilder::new();
        create.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut create, 0o700);
        create.create(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
        hook::install();
        let events = Arc::new(Events::default());
        os::watch(&dir, events.clone())?;
        let answers = Answers::new(&dir);
        Ok(Live { switch, dir, answers, events, looked: None, taken: VecDeque::new() })
    }

    /// The generation the program runs.
    pub fn generation(&self) -> u64 {
        self.switch.current()
    }

    /// Takes every message that came since the last call, in order, answers each, and says
    /// what became of the last. Call it where nothing of the program's is half done, such as
    /// between two frames: code is only written here, and a function running when its patch
    /// lands finishes as it was. While nothing came it is one atomic load.
    pub fn poll(&mut self) -> Option<Outcome> {
        let mut last = None;
        loop {
            let events = self.events.count.load(Ordering::Acquire);
            let generation = self.switch.current();
            if self.looked == Some((events, generation)) {
                return last;
            }
            self.looked = Some((events, generation));
            if let Some(outcome) = self.next(generation) {
                last = Some(outcome);
            }
        }
    }

    /// Takes the message for `generation`, if one came that wasn't taken.
    fn next(&mut self, generation: u64) -> Option<Outcome> {
        let path = self.dir.join(message_file(generation));
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => return Some(self.unread(generation, &path, &error.to_string())),
        };
        let meta = match file.metadata() {
            Ok(meta) => meta,
            Err(error) => return Some(self.unread(generation, &path, &error.to_string())),
        };
        let identity = Identity::of(&meta);
        if self.taken.contains(&(generation, identity)) {
            return None;
        }
        if self.taken.len() == KEPT {
            self.taken.pop_front();
        }
        self.taken.push_back((generation, identity));
        let map = match os::Map::new(&file, meta.len()) {
            Ok(map) => map,
            Err(why) => return Some(self.unread(generation, &path, &why)),
        };
        drop(file);
        let (outcome, said) = match Message::view(map.bytes()) {
            Ok(message) => self.switch.take(&message),
            Err(why) => {
                let why = format!("{}: {why}", path.display());
                (
                    Outcome::Declined { generation, why: why.clone() },
                    Some(Answer::Declined { generation, why }),
                )
            }
        };
        drop(map);
        if let Some(said) = &said {
            self.answers.write(said);
        }
        Some(outcome)
    }

    /// Declines the message file at `path`, which could not be read.
    fn unread(&mut self, generation: u64, path: &Path, why: &str) -> Outcome {
        let why = format!("{}: {why}", path.display());
        self.answers.write(&Answer::Declined { generation, why: why.clone() });
        Outcome::Declined { generation, why }
    }

    /// Runs `f`, such as one frame of the program, under `catch_unwind`. A panic with a frame
    /// in the code of a generation the program runs rolls the newest such generation back to
    /// the one it was applied on, is answered, and comes back as
    /// [`Outcome::RolledBack`]; any other panic is resumed as it was. What `f` changed before
    /// it panicked stays changed: the guard is where the program says that is fine.
    pub fn guard<R>(&mut self, f: impl FnOnce() -> R) -> Result<R, Outcome> {
        let answers = &mut self.answers;
        self.switch.guard(f, &mut |said| answers.write(said))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.events.stop.store(true, Ordering::Relaxed);
    }
}

/// The directory the descriptor names.
fn directory(layout: &Layout) -> Result<PathBuf, String> {
    let [at, len] = layout.dir;
    if len == 0 {
        return Err(format!("{DESCRIPTOR} names no directory to take patches through"));
    }
    // SAFETY: the linker wrote the path in the image's read-only data, `len` bytes (at most
    // `MOST_DIR`, `Layout::new` checked) at `at`, mapped as long as the image is.
    let bytes = unsafe { std::slice::from_raw_parts(layout.slid(at) as *const u8, len as usize) };
    let path = std::str::from_utf8(bytes)
        .map_err(|_| format!("{DESCRIPTOR} names a directory that is not UTF-8"))?;
    Ok(PathBuf::from(path))
}
