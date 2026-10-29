//! blaze's sans-IO core ([docs/v2.md](../../../docs/v2.md#the-io-core-blaze-core)).
//!
//! A state machine of blaze's (the build's plane, a journal, the store's index) does no IO,
//! reads no clock and starts no thread. It asks for IO by pushing [`Effect`]s, each with a
//! [`Tag`], and is told how each went by a [`Completion`] with the same tag. A [`Driver`] runs
//! the effects: [`sim::Sim`] in one thread under a seeded scheduler that reorders, fails and
//! crashes (deterministic simulation tests), [`threads::Threads`] on the real file system, and
//! on Linux [`uring::Uring`], on one io_uring.
//!
//! Files are named relative to the driver's root, so the same state machine runs in a
//! simulation, a temporary directory or the daemon's state directory unchanged.

mod fs;
pub mod sim;
pub mod threads;
#[cfg(target_os = "linux")]
pub mod uring;
#[cfg(target_os = "linux")]
mod watch;

use std::{fmt, time::Duration};

/// Names a request so its completion can be matched to it. The state machine chooses tags; a
/// driver hands them back unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Tag(pub u64);

/// A file under the driver's root: `/`-separated, relative, without `.` or `..`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct FileName(Box<str>);

impl FileName {
    /// `None` for a name that would leave the driver's root or names nothing.
    pub fn new(name: &str) -> Option<FileName> {
        let ok = !name.is_empty()
            && !name.starts_with('/')
            && name.split('/').all(|it| !it.is_empty() && it != "." && it != "..");
        ok.then(|| FileName(name.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `self` is `dir` or under it.
    pub fn is_under(&self, dir: &FileName) -> bool {
        self.0 == dir.0
            || self.0.strip_prefix(&*dir.0).is_some_and(|rest| rest.starts_with('/'))
    }
}

impl fmt::Display for FileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// IO a state machine asks for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    /// The whole file. A file that is not there reads as empty: a journal not yet written is
    /// one with no records.
    ReadAll { file: FileName, tag: Tag },
    /// `len` bytes at `at`, or fewer at the end of the file.
    Read { file: FileName, at: u64, len: usize, tag: Tag },
    /// Bytes added at the end of the file, creating it. Not durable until a [`Effect::Sync`] of
    /// the file asked for after this one has completed.
    Append { file: FileName, bytes: Vec<u8>, tag: Tag },
    /// Everything appended to the file before this was asked for is durable once it completes.
    Sync { file: FileName, tag: Tag },
    /// The file cut to its first `len` bytes (made if missing); durable once a later sync
    /// completes.
    Truncate { file: FileName, len: u64, tag: Tag },
    /// The file's whole contents replaced by `bytes`, durably and all at once: a reader or a
    /// crash sees the old contents or the new, never a part. Its directories are made.
    Write { file: FileName, bytes: Vec<u8>, tag: Tag },
    /// `to` made a file with `from`'s contents that shares its blocks (`FICLONE`,
    /// `clonefile`). Fails where the file system cannot share blocks, naming it: nothing
    /// copies instead.
    Clone { from: FileName, to: FileName, tag: Tag },
    /// `to` made another name of `from`, for outputs nothing writes in place.
    Link { from: FileName, to: FileName, tag: Tag },
    /// What the file is, or that it is not there.
    Stat { file: FileName, tag: Tag },
    /// The file, or an empty directory, gone. Gone already is done.
    Remove { file: FileName, tag: Tag },
    /// Completes with [`Done::Changed`] each time files under `dir` change, until an
    /// [`Effect::Unwatch`] of the same tag, which ends it with [`Done::Unwatched`], or a
    /// [`Done::Failed`], which ends it too. It is in flight until it ends; an unwatch has no
    /// completion of its own but the watch's end.
    Watch { dir: FileName, tag: Tag },
    Unwatch { tag: Tag },
    /// Completes once `after` has passed.
    Timer { after: Duration, tag: Tag },
}

impl Effect {
    pub fn tag(&self) -> Tag {
        match self {
            Effect::ReadAll { tag, .. }
            | Effect::Read { tag, .. }
            | Effect::Append { tag, .. }
            | Effect::Sync { tag, .. }
            | Effect::Truncate { tag, .. }
            | Effect::Write { tag, .. }
            | Effect::Clone { tag, .. }
            | Effect::Link { tag, .. }
            | Effect::Stat { tag, .. }
            | Effect::Remove { tag, .. }
            | Effect::Watch { tag, .. }
            | Effect::Unwatch { tag }
            | Effect::Timer { tag, .. } => *tag,
        }
    }
}

/// What [`Effect::Stat`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
    pub len: u64,
    pub dir: bool,
    /// Nanoseconds since the epoch of its last change, where the file system keeps it; 0 in a
    /// simulation, which has no clock of the world.
    pub modified_ns: u64,
}

/// How an [`Effect`] went.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Done {
    Read(Vec<u8>),
    Appended,
    Synced,
    Truncated,
    Written,
    Cloned,
    Linked,
    Stat(Option<Meta>),
    Removed,
    /// Files under a watched directory changed: each is named once, in no order. `lost` says
    /// the system dropped changes (its queue overflowed), so whoever watches must look at the
    /// whole directory again: the names are not all of them.
    Changed { files: Vec<FileName>, lost: bool },
    Unwatched,
    Fired,
    /// It did not happen. For an append, none of its bytes are durable unless a later sync
    /// says so; for a sync, nothing appended since the last successful one is known durable;
    /// for a write, the file is as it was.
    Failed(IoError),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IoError(pub String);

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Completion {
    pub tag: Tag,
    pub done: Done,
}

/// Runs effects and reports their completions, in any order. Every method is called from the
/// one thread that runs the state machines.
pub trait Driver {
    /// Starts every effect in `effects`, leaving it empty.
    fn submit(&mut self, effects: &mut Vec<Effect>);
    /// Moves completions into `out`, waiting up to `wait` for the first if none is ready.
    /// Returns how many it moved.
    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize;
    /// How many effects were submitted and have not completed.
    fn in_flight(&self) -> usize;
    /// Monotonic time since the driver began.
    fn now(&self) -> Duration;
}

/// Runs `effects` and polls until every one of them has completed, handing each completion to
/// `on`, which may push more effects (submitted before the next poll). For drivers whose
/// effects all complete; a timer far ahead holds it until it fires, and a watch until it is
/// unwatched.
pub fn run_until_idle(
    driver: &mut dyn Driver,
    effects: &mut Vec<Effect>,
    mut on: impl FnMut(Completion, &mut Vec<Effect>),
) {
    let mut done = Vec::new();
    loop {
        driver.submit(effects);
        if driver.in_flight() == 0 {
            return;
        }
        driver.poll(&mut done, Duration::from_secs(60));
        for completion in done.drain(..) {
            on(completion, effects);
        }
    }
}

#[cfg(test)]
mod tests;
