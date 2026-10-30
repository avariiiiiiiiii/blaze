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
//! generation `g` takes the message in the file
//! [`message_file`](crate::patch::message_file)`(g)` of the directory it
//! names, read into a buffer kept from message to message, and answers each there
//! ([`crate::patch::Answer`]). A thread blocks on the platform's notification of changes to
//! the directory (inotify, kqueue, change notifications) and counts them, so [`Live::poll`]
//! is one atomic load while nothing came.
//!
//! An image linked with an execution log ([`exec`]) keeps its cell on the log's table, and a
//! switch stores only the header's current table; [`publish`] writes one slot into every
//! generation's table, for lazy compilation's code placed after the link.
//!
//! Linux on x86-64 and AArch64, macOS on AArch64 and Windows on x86-64; elsewhere
//! [`Live::start`] says so.

use std::{
    collections::VecDeque,
    fmt,
    fs::{File, Metadata},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::Thread,
    time::SystemTime,
};

use crate::patch::{
    ANSWER, Answer, DESCRIPTOR, DESCRIPTOR_MAGIC, Descriptor, Message, message_file_into,
};
pub use exec::publish;
use os::backtrace;
use switch::{KEPT, Layout, Switch};

mod column;
pub mod exec;
mod hook;
mod image;
#[cfg(all(test, blaze_check))]
mod model;
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
    use std::{ops::Range, path::Path, sync::Arc};

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

        fn cell(&self, _: u64) -> u64 {
            match *self {}
        }

        fn in_image(&self, _: &Range<u64>) -> Result<(), String> {
            match *self {}
        }

        fn bytes(&self, _: &Range<u64>) -> Result<&'static [u8], String> {
            match *self {}
        }

        fn loaders(&self, _: u64, _: u64) -> &[std::sync::atomic::AtomicU64] {
            match *self {}
        }

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

    pub(super) fn descriptor() -> Result<(u64, &'static [u8]), String> {
        Err(PLATFORMS.to_owned())
    }

    pub(super) fn in_image_of(_: &Range<u64>, _: u64) -> Result<(), String> {
        Err(PLATFORMS.to_owned())
    }

    pub(super) fn watch(_: &Path, _: Arc<Events>) -> Result<(), String> {
        Err(PLATFORMS.to_owned())
    }

    pub(super) fn backtrace(_: &mut Vec<u64>) {}

    pub(super) const GRAIN: usize = 64 << 10;

    /// No address space is reserved here: the switch's columns, and so the switch, can't be
    /// made.
    pub(super) enum Reserved {}

    impl Reserved {
        pub(super) fn new(_: usize) -> Result<Reserved, String> {
            // alloc: error path, once at start on a platform it doesn't run on
            Err(PLATFORMS.to_owned())
        }

        pub(super) fn base(&self) -> u64 {
            match *self {}
        }

        pub(super) fn len(&self) -> usize {
            match *self {}
        }

        pub(super) fn committed(&self) -> usize {
            match *self {}
        }

        pub(super) fn commit(&mut self, _: usize) -> Result<(), String> {
            match *self {}
        }
    }
}

/// What became of a message, or of a panic under [`Live::guard`].
#[derive(PartialEq, Eq, Debug)]
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
    /// The thread of [`Live::apply_as_they_come`], woken at each event.
    waiter: OnceLock<Thread>,
}

impl Events {
    pub(crate) fn tell(&self) {
        self.count.fetch_add(1, Ordering::Release);
        if let Some(waiter) = self.waiter.get() {
            waiter.unpark();
        }
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
            // alloc: setup, once at start
            partial: dir.join(format!("{ANSWER}.{}", std::process::id())),
            // alloc: grown to the largest answer once, then reused for every answer
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
    /// The bytes of the last message, its buffer kept for the next.
    message: Vec<u8>,
    /// The path of the last message file looked for, and its name, kept for the next.
    path: PathBuf,
    name: String,
}

impl Live {
    /// Finds this image's descriptor, readies its patch code region for writes, installs the
    /// panic hook [`Live::guard`] needs and starts watching for messages. It reserves about
    /// 435 MiB of address space for the log of generations, which costs memory only as
    /// patches fill it; under a `ulimit -v` that leaves less, it fails naming the call and
    /// the size.
    pub fn start() -> Result<Live, String> {
        if STARTED.swap(true, Ordering::AcqRel) {
            // alloc: error path
            return Err("blaze-live's slots were started already in this program".to_owned());
        }
        let live = Live::open();
        if live.is_err() {
            STARTED.store(false, Ordering::Release);
        }
        live
    }

    fn open() -> Result<Live, String> {
        let (at, bytes) = os::descriptor()?;
        let descriptor = Descriptor::decode(bytes).ok_or_else(|| {
            // alloc: error path
            format!(
                "{DESCRIPTOR} at {at:#x} does not start with {}",
                // alloc: error path
                String::from_utf8_lossy(&DESCRIPTOR_MAGIC)
            )
        })?;
        let layout = Layout::new(&descriptor, at)?;
        let memory = os::Os::new(&layout)?;
        let dir = directory(&layout, &memory)?;
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let sampled = {
            let (slide, image) = (layout.slide, memory.image());
            crate::sample::Code {
                slide,
                image: image.start.wrapping_sub(slide)..image.end.wrapping_sub(slide),
                // clone: setup, a Range<u64> of two words, once at start
                patches: layout.code.clone(),
            }
        };
        let switch = Switch::new(memory, layout)?;
        let mut create = std::fs::DirBuilder::new();
        create.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut create, 0o700);
        // alloc: error path
        create.create(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
        // clone: setup, the directory handed once to `DIR`
        _ = crate::DIR.set(dir.clone());
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        // clone: setup, the directory handed once to the sampler's thread
        crate::sample::start(dir.clone(), sampled);
        hook::install();
        // alloc: setup, the one `Events` the watcher's thread shares
        let events = Arc::new(Events::default());
        // clone: setup, the watcher thread's handle, once
        os::watch(&dir, events.clone())?;
        let answers = Answers::new(&dir);
        // alloc: grown to a message file's name once, then reused
        let mut name = String::new();
        message_file_into(switch.current(), &mut name);
        let path = dir.join(&name);
        Ok(Live {
            switch,
            dir,
            answers,
            events,
            looked: None,
            taken: VecDeque::with_capacity(KEPT),
            // alloc: grown to the largest message once, then reused for every message
            message: Vec::new(),
            path,
            name,
        })
    }

    /// The generation the program runs.
    pub fn generation(&self) -> u64 {
        self.switch.current()
    }

    /// Takes every message that came since the last call, in order, answers each, and says
    /// what became of the last. Call it where nothing of the program's is half done, such as
    /// between two frames. Code is only written here, never over code a thread can be in, so
    /// a function running on any thread when its patch lands finishes as it was. Data a patch
    /// changes in place (a static's new value) is written with plain stores, so no other
    /// thread may use it while this runs. While nothing came it is one atomic load.
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

    /// Takes each message as soon as it comes, on a thread of its own woken by the watcher,
    /// instead of at [`Live::poll`], and tells `told` what became of it. For a program whose
    /// functions can change between any two calls: one of many functions that work together
    /// can be in the middle of a frame when some of them do, and data a patch changes in place
    /// is stored while other threads may use it, as [`Live::poll`] says.
    pub fn apply_as_they_come(mut self, mut told: impl FnMut(Outcome) + Send + 'static) {
        let applying = move || {
            _ = self.events.waiter.set(std::thread::current());
            loop {
                if let Some(outcome) = self.poll() {
                    told(outcome);
                }
                std::thread::park();
            }
        };
        std::thread::Builder::new()
            // alloc: setup, the thread's name, once
            .name("blaze-live-apply".to_owned())
            .spawn(applying)
            .expect("spawning the thread that applies patches");
    }

    /// Takes the message for `generation`, if one came that wasn't taken.
    fn next(&mut self, generation: u64) -> Option<Outcome> {
        message_file_into(generation, &mut self.name);
        self.path.set_file_name(&self.name);
        let path = std::mem::take(&mut self.path);
        let outcome = self.read_message(generation, &path);
        self.path = path;
        outcome
    }

    /// Takes the message file at `path`, for `generation`, unless it was taken already.
    fn read_message(&mut self, generation: u64, path: &Path) -> Option<Outcome> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            // alloc: error path
            Err(error) => return Some(self.unread(generation, path, &error.to_string())),
        };
        let meta = match file.metadata() {
            Ok(meta) => meta,
            // alloc: error path
            Err(error) => return Some(self.unread(generation, path, &error.to_string())),
        };
        let identity = Identity::of(&meta);
        if self.taken.contains(&(generation, identity)) {
            return None;
        }
        if self.taken.len() == KEPT {
            self.taken.pop_front();
        }
        self.taken.push_back((generation, identity));
        // Read, not mapped: the bytes checked are the bytes written, whatever happens to the
        // file meanwhile.
        // One `read` of the length the file has: `take` ends the read there, with no second
        // `read` to find the end.
        self.message.clear();
        self.message.reserve(usize::try_from(meta.len()).unwrap_or(0));
        if let Err(error) = (&file).take(meta.len()).read_to_end(&mut self.message) {
            // alloc: error path
            return Some(self.unread(generation, path, &error.to_string()));
        }
        drop(file);
        let (outcome, said) = match Message::view(&self.message) {
            Ok(message) => self.switch.take(&message),
            Err(why) => {
                // alloc: error path
                let why = format!("{}: {why}", path.display());
                (
                    // alloc: error path
                    Outcome::Declined { generation, why: why.clone() },
                    Some(Answer::Declined { generation, why }),
                )
            }
        };
        if let Some(said) = &said {
            self.answers.write(said);
        }
        Some(outcome)
    }

    /// Declines the message file at `path`, which could not be read.
    fn unread(&mut self, generation: u64, path: &Path, why: &str) -> Outcome {
        // alloc: error path
        let why = format!("{}: {why}", path.display());
        // alloc: error path
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

/// The directory the descriptor names: `len` bytes (at most `MOST_DIR`, `Layout::new`
/// checked) of the image's read-only data.
fn directory(layout: &Layout, memory: &os::Os) -> Result<PathBuf, String> {
    let [at, len] = layout.dir;
    if len == 0 {
        // alloc: error path
        return Err(format!("{DESCRIPTOR} names no directory to take patches through"));
    }
    let at = layout.slid(at);
    let end = at
        .checked_add(len)
        // alloc: error path
        .ok_or_else(|| format!("{DESCRIPTOR} names a directory past the end of memory"))?;
    let bytes = memory.bytes(&(at..end))?;
    let path = std::str::from_utf8(bytes)
        // alloc: error path
        .map_err(|_| format!("{DESCRIPTOR} names a directory that is not UTF-8"))?;
    Ok(PathBuf::from(path))
}
