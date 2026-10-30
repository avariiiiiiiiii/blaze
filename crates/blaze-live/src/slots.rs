//! The runtime of a program blaze-link linked: it takes patches while it runs.
//!
//! Every function a patch can replace is called through a slot of a jump table: a call loads
//! the table cell, then the slot. A patch writes its code and data where the linker placed
//! them, then publishes a new table with its slots changed in one atomic store to the cell,
//! so every thread's next call through any slot sees all of the patch or none of it. Each
//! generation's table is kept (the last [`KEPT`](switch::KEPT)), and going back to one is one
//! store again: when the linker says so ([`crate::patch::Message::RollBack`]), or when code of
//! a generation panics under [`Live::guard`].
//!
//! The descriptor ([`crate::patch::Descriptor`]) says where everything is; messages come
//! through files in the directory it names, and each is answered there
//! ([`crate::patch::Answer`]).
//!
//! Linux on x86-64 and AArch64, macOS on AArch64 and Windows on x86-64; elsewhere
//! [`Live::start`] says so.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

use crate::patch::{
    ANSWER, Answer, DESCRIPTOR, DESCRIPTOR_MAGIC, Descriptor, Message, message_file,
};
use os::backtrace;
use switch::{Layout, Switch};

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
    use super::switch::{Dwarf, Layout, Memory};

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

        fn code(&mut self, _: &[(u64, Vec<u8>)]) -> Result<(), String> {
            match *self {}
        }

        fn data(&mut self, _: &[(u64, Vec<u8>)]) -> Result<(), String> {
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

    pub(super) fn descriptor() -> Result<u64, String> {
        Err(PLATFORMS.to_owned())
    }

    pub(super) fn backtrace(_: &mut Vec<u64>) {}
}

/// How often the directory is looked at for the next message.
const LOOK_EVERY: Duration = Duration::from_millis(5);

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

/// A program taking patches.
pub struct Live {
    switch: Switch<os::Os>,
    dir: PathBuf,
    messages: Receiver<(u64, Result<Message, String>)>,
    stop: Arc<AtomicBool>,
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
        let (messages, stop) = watch(dir.clone())?;
        Ok(Live { switch, dir, messages, stop })
    }

    /// The generation the program runs.
    pub fn generation(&self) -> u64 {
        self.switch.current()
    }

    /// Takes every message that came since the last call, in order, answers each, and says
    /// what became of the last. Call it where nothing of the program's is half done, such as
    /// between two frames: code is only written here, and a function running when its patch
    /// lands finishes as it was.
    pub fn poll(&mut self) -> Option<Outcome> {
        let mut last = None;
        while let Ok((sequence, message)) = self.messages.try_recv() {
            let (outcome, said) = match message {
                Ok(message) => self.switch.handle(message),
                Err(why) => {
                    let generation = self.switch.current();
                    let why = format!("message {sequence}: {why}");
                    (
                        Outcome::Declined { generation, why: why.clone() },
                        Some(Answer::Declined { generation, why }),
                    )
                }
            };
            if let Some(said) = said {
                answer(&self.dir, &said);
            }
            last = Some(outcome);
        }
        last
    }

    /// Runs `f`, such as one frame of the program, under `catch_unwind`. A panic with a frame
    /// in the code of a generation the program runs rolls the newest such generation back to
    /// the one it was applied on, is answered, and comes back as
    /// [`Outcome::RolledBack`]; any other panic is resumed as it was. What `f` changed before
    /// it panicked stays changed: the guard is where the program says that is fine.
    pub fn guard<R>(&mut self, f: impl FnOnce() -> R) -> Result<R, Outcome> {
        let dir = &self.dir;
        self.switch.guard(f, &mut |said| answer(dir, said))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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

/// Reads the messages of `dir` on a thread of its own, in sequence order, each once its file
/// is there, until the flag says stop.
#[allow(clippy::type_complexity)]
fn watch(
    dir: PathBuf,
) -> Result<(Receiver<(u64, Result<Message, String>)>, Arc<AtomicBool>), String> {
    let (send, receive) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let watching = move || {
        let mut sequence = 0;
        while !stopped.load(Ordering::Relaxed) {
            let path = dir.join(message_file(sequence));
            let message = match std::fs::read(&path) {
                Ok(bytes) => {
                    Message::decode(&bytes).map_err(|why| format!("{}: {why}", path.display()))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::thread::sleep(LOOK_EVERY);
                    continue;
                }
                Err(error) => Err(format!("{}: {error}", path.display())),
            };
            if send.send((sequence, message)).is_err() {
                return;
            }
            sequence += 1;
        }
    };
    std::thread::Builder::new()
        .name("blaze-live-slots".to_owned())
        .spawn(watching)
        .map_err(|it| format!("starting the thread that watches for patches: {it}"))?;
    Ok((receive, stop))
}

/// Writes `answer` as the file [`ANSWER`] of `dir`, replacing the last in one rename.
fn answer(dir: &Path, answer: &Answer) {
    let path = dir.join(ANSWER);
    let partial = dir.join(format!("{ANSWER}.{}", std::process::id()));
    let written =
        std::fs::write(&partial, answer.encode()).and_then(|()| std::fs::rename(&partial, &path));
    if let Err(error) = written {
        eprintln!("blaze-live: answering in {}: {error}", path.display());
    }
}
