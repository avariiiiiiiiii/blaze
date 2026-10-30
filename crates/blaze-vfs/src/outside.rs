//! What changed outside every view (docs/v2/plan/lanes/vfs/outside.md): the directories user
//! code reads that no view holds (the toolchain, the C compiler's, `/usr/include`, each traced
//! path's parent), watched so a check asks "has anything under this changed since then?" of a
//! table, not of the disk.
//!
//! - [`Dirs`] is the table, one per process ([`dirs`]): a generation counter bumped by every
//!   change heard and every watch confirmed, and for each directory two atomics, `since` (the
//!   generation it has been watched without a break since; [`UNCONFIRMED`] while it is not)
//!   and `changed` (the generation of the last change heard under it). A read ([`generation`],
//!   [`unchanged_since`]) is acquire loads only: no lock, no shared write, no allocation.
//! - [`want`] adds a directory to watch, once per directory per process.
//! - [`Outside`] is the sans-IO driver of the watches, as [`crate::Watcher`] is: a wanted
//!   directory becomes an [`Effect::Watch`], confirmed by an [`Effect::Stat`] of it asked
//!   right after, which every driver runs after the watch is in place (the order
//!   [`crate::Watcher::start`] relies on: each driver sets a watch up as it is submitted).
//!   A change bumps the generation and marks its directory; an end, a failure or a change
//!   naming the directory itself (it was replaced or moved, or may have been) stops it
//!   vouching at once.
//! - Events come after the change that made them (FSEvents' queue, a thread reading inotify),
//!   so a round of [`Effect::Flush`]es ([`Outside::flush`]) is the barrier: until every flush
//!   of the round is in, nothing vouches, and once it is, every change made before the round
//!   began has been heard. The daemon runs one round per request, before its checks.
//!
//! Sound: [`unchanged_since`]`(path, g)` is true only when some directory at or above `path`
//! was confirmed before `g` was read (`since <= g`: its watch was in place before whatever the
//! caller looked at after reading `g`), heard nothing after (`changed <= g`), and the last
//! round is in. A directory the driver's root cannot name is never confirmed: each check of
//! it looks at the disk, which is the accurate answer.

use std::{
    hash::Hasher as _,
    path::{Path, PathBuf},
};

use blaze_core::{Completion, Done, Effect, FileName, Tag};
use rustc_hash::FxHasher;

use crate::sync::{
    Mutex, OnceLock, PoisonError,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

/// The `since` of a directory not watched without a break: above every generation, so it
/// vouches for none.
pub const UNCONFIRMED: u64 = u64::MAX;

/// Chunk `k` of the directories holds `FIRST << k`, so an index's place is arithmetic.
const FIRST: usize = 64;
const CHUNKS: usize = 20;
/// Table `k` of the index has `TABLE << k` places, at most half full.
const TABLE: usize = 64;
const TABLES: usize = 21;
/// The most directories: what the chunks hold, and what a tag range of 2^32 numbers in fours.
const MOST: u32 = (FIRST * ((1 << CHUNKS) - 1)) as u32;

/// A directory watched: its path, as it was wanted, and its two generations.
pub struct Dir {
    path: Box<Path>,
    since: AtomicU64,
    changed: AtomicU64,
}

impl Dir {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The generation it has been watched without a break since, or [`UNCONFIRMED`].
    pub fn since(&self) -> u64 {
        self.since.load(Ordering::Acquire)
    }

    /// The generation of the last change heard under it.
    pub fn changed(&self) -> u64 {
        self.changed.load(Ordering::Acquire)
    }
}

/// The counters every check reads, on a line pair of their own: the driver writes them once
/// per change heard, never per check.
#[repr(align(128))]
struct Hot {
    generation: AtomicU64,
    /// The last round of flushes is in: false while one is out, and until the first is in.
    settled: AtomicBool,
}

/// The directories watched outside every view, append-only: found by an `FxHash` of their
/// bytes in an open-addressed index of `u64` places (the hash's high half and the index plus
/// one, 0 empty), grown by copying into a table twice the size, the old ones kept for readers
/// still probing them (as [`blaze_ids::Sharded`] does). A lookup compares the bytes, so two
/// paths of one hash are two directories.
pub struct Dirs {
    hot: Hot,
    /// One [`Outside`] drives the table at a time.
    driven: AtomicBool,
    /// How many directories are published; each below it is in `slots`.
    len: AtomicU32,
    /// Which of `tables` is in use.
    level: AtomicU32,
    slots: [OnceLock<Box<[OnceLock<Dir>]>>; CHUNKS],
    tables: [OnceLock<Box<[AtomicU64]>>; TABLES],
    /// Held to add a directory: cold, once per directory per process.
    adding: Mutex<()>,
}

fn chunk_of(raw: u32) -> (usize, usize) {
    let biased = raw as usize / FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, raw as usize - FIRST * ((1 << chunk) - 1))
}

/// Where a key's probe starts and the half of its hash a place keeps.
fn tag(key: &[u8]) -> u32 {
    let mut hasher = FxHasher::default();
    hasher.write(key);
    (hasher.finish() >> 32) as u32
}

fn entry(tag: u32, raw: u32) -> u64 {
    (tag as u64) << 32 | (raw as u64 + 1)
}

fn is_separator(byte: u8) -> bool {
    byte.is_ascii() && std::path::is_separator(byte as char)
}

impl Dirs {
    pub const fn new() -> Dirs {
        Dirs {
            hot: Hot { generation: AtomicU64::new(0), settled: AtomicBool::new(false) },
            driven: AtomicBool::new(false),
            len: AtomicU32::new(0),
            level: AtomicU32::new(0),
            slots: [const { OnceLock::new() }; CHUNKS],
            tables: [const { OnceLock::new() }; TABLES],
            adding: Mutex::new(()),
        }
    }

    /// The generation now: one load. A caller stores it with what it looked at after, and asks
    /// [`Dirs::unchanged_since`] with it later.
    pub fn generation(&self) -> u64 {
        self.hot.generation.load(Ordering::Acquire)
    }

    /// Whether nothing at `path` can have changed since `generation` was read: a directory at
    /// or above it has been watched without a break since before then and heard of no change
    /// under it after, and the last round of flushes is in. False is "look at the disk".
    pub fn unchanged_since(&self, path: &Path, generation: u64) -> bool {
        if !self.hot.settled.load(Ordering::Acquire) {
            return false;
        }
        let mut bytes = path.as_os_str().as_encoded_bytes();
        while bytes.len() > 1 && is_separator(bytes[bytes.len() - 1]) {
            bytes = &bytes[..bytes.len() - 1];
        }
        loop {
            if let Some(dir) = self.find(bytes, tag(bytes))
                && dir.since.load(Ordering::Acquire) <= generation
                && dir.changed.load(Ordering::Acquire) <= generation
            {
                return true;
            }
            // The file system's root is never watched: the walk ends below it.
            match bytes.iter().rposition(|&it| is_separator(it)) {
                Some(at) if at > 0 => bytes = &bytes[..at],
                _ => return false,
            }
        }
    }

    /// Adds `dir` to watch, if it is not there already: the driver watches it from its next
    /// [`Outside::start`] or round. The file system's root is not watched (a watch of every
    /// file on the machine would hear every write): a path that only it covers is looked at
    /// on the disk at each check.
    pub fn want(&self, dir: &Path) {
        let dir = dir.components().as_path();
        if dir.parent().is_none() {
            return;
        }
        let key = dir.as_os_str().as_encoded_bytes();
        let tag = tag(key);
        if self.find(key, tag).is_some() {
            return;
        }
        let _adding = self.adding.lock().unwrap_or_else(PoisonError::into_inner);
        if self.find(key, tag).is_some() {
            return;
        }
        let raw = self.len.load(Ordering::Relaxed);
        assert!(raw < MOST, "more than {MOST} directories watched outside every view");
        let (chunk, at) = chunk_of(raw);
        let slots =
            self.slots[chunk].get_or_init(|| (0..FIRST << chunk).map(|_| OnceLock::new()).collect());
        let dir = Dir {
            path: dir.to_path_buf().into_boxed_path(),
            since: AtomicU64::new(UNCONFIRMED),
            changed: AtomicU64::new(0),
        };
        if slots[at].set(dir).is_err() {
            unreachable!("directory {raw} added twice");
        }
        let mut level = self.level.load(Ordering::Relaxed) as usize;
        if (raw as usize + 1) * 2 > TABLE << level {
            level = self.grow(level);
        }
        let table = self.table(level);
        let mask = table.len() - 1;
        let mut place = tag as usize & mask;
        while table[place].load(Ordering::Relaxed) != 0 {
            place = (place + 1) & mask;
        }
        table[place].store(entry(tag, raw), Ordering::Release);
        self.len.store(raw + 1, Ordering::Release);
    }

    /// How many directories are wanted.
    pub fn len(&self) -> u32 {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The directory numbered `at`, below [`Dirs::len`].
    pub fn dir(&self, at: u32) -> &Dir {
        self.slot(at).expect("a directory below the table's length")
    }

    fn slot(&self, raw: u32) -> Option<&Dir> {
        let (chunk, at) = chunk_of(raw);
        self.slots[chunk].get()?.get(at)?.get()
    }

    /// The table in use, made if there is none yet: for the writer only.
    fn table(&self, level: usize) -> &[AtomicU64] {
        self.tables[level].get_or_init(|| (0..TABLE << level).map(|_| AtomicU64::new(0)).collect())
    }

    /// No lock, no write and no allocation: an index with no table yet holds nothing.
    fn find(&self, key: &[u8], tag: u32) -> Option<&Dir> {
        let level = self.level.load(Ordering::Acquire) as usize;
        let table = self.tables[level].get()?;
        let mask = table.len() - 1;
        let mut place = tag as usize & mask;
        loop {
            match table[place].load(Ordering::Acquire) {
                0 => return None,
                entry if (entry >> 32) as u32 == tag => {
                    let dir = self.slot(entry as u32 - 1).expect("an index entry before its directory");
                    if dir.path.as_os_str().as_encoded_bytes() == key {
                        return Some(dir);
                    }
                }
                _ => {}
            }
            place = (place + 1) & mask;
        }
    }

    /// Moves the index into a table twice its size and makes it the one in use, from the
    /// entries alone. Called while adding.
    fn grow(&self, level: usize) -> usize {
        assert!(level + 1 < TABLES, "an index of more than {MOST} directories");
        let (old, new) = (self.table(level), self.table(level + 1));
        let mask = new.len() - 1;
        for entry in old {
            let entry = entry.load(Ordering::Relaxed);
            if entry == 0 {
                continue;
            }
            let mut place = (entry >> 32) as usize & mask;
            while new[place].load(Ordering::Relaxed) != 0 {
                place = (place + 1) & mask;
            }
            new[place].store(entry, Ordering::Relaxed);
        }
        self.level.store(level as u32 + 1, Ordering::Release);
        level + 1
    }

    /// A generation after every one read so far.
    fn bump(&self) -> u64 {
        self.hot.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn settle(&self, settled: bool) {
        self.hot.settled.store(settled, Ordering::Release);
    }
}

impl Default for Dirs {
    fn default() -> Dirs {
        Dirs::new()
    }
}

static DIRS: Dirs = Dirs::new();

/// The one table of the process.
pub fn dirs() -> &'static Dirs {
    &DIRS
}

/// [`Dirs::generation`] of the process's table.
pub fn generation() -> u64 {
    DIRS.generation()
}

/// [`Dirs::unchanged_since`] of the process's table.
pub fn unchanged_since(path: &Path, generation: u64) -> bool {
    DIRS.unchanged_since(path, generation)
}

/// [`Dirs::want`] of the process's table.
pub fn want(dir: &Path) {
    DIRS.want(dir)
}

/// How many tags a driver uses: four per directory (its watch, its stat, its flush).
pub const TAGS: u64 = 1 << 32;
const WATCH: u64 = 0;
const STAT: u64 = 1;
const FLUSH: u64 = 2;
/// The most rounds a directory whose watch or stat failed waits before it is asked again; the
/// wait doubles from one with each failure in a row.
const MOST_BACKOFF: u64 = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// No watch in flight: never asked (or not nameable), failed, or ended.
    Idle,
    Watching,
    /// Its unwatch is asked: it is being watched again.
    Ending,
}

/// The driver's own state of a directory.
struct Watch {
    /// As the driver names it; `None` for one its root cannot name, never watched.
    file: Option<FileName>,
    state: State,
    /// Its stat is in flight.
    stat: bool,
    /// The round its flush in flight was asked in.
    flush: Option<u64>,
    /// It owes the round under way a flush asked in it.
    due: bool,
    /// Its watch was in place when its stat was answered, and has not broken since.
    confirmed: bool,
    /// Its watch is to be made again once nothing of it is in flight.
    restart: bool,
    /// The round before which a failed watch or stat is not asked again, and the next wait.
    retry: u64,
    backoff: u64,
}

/// The sans-IO driver of the watches of a [`Dirs`]: written as [`crate::Watcher`] is, it asks
/// with tags `tags..tags + `[`TAGS`] ([`Outside::owns`]) and is told of each with
/// [`Outside::complete`].
pub struct Outside<'d> {
    dirs: &'d Dirs,
    /// The driver's root: what the directories are named under.
    root: PathBuf,
    tags: u64,
    watches: Vec<Watch>,
    round: u64,
    /// The flushes the round under way waits for.
    left: u32,
}

impl<'d> Outside<'d> {
    /// The driver of `dirs`' watches, through a driver whose files are under `root` (`/`, so
    /// that every directory is nameable). One drives a table at a time.
    pub fn new(dirs: &'d Dirs, root: impl Into<PathBuf>, tags: u64) -> Outside<'d> {
        assert!(!dirs.driven.swap(true, Ordering::AcqRel), "another Outside drives this table");
        Outside { dirs, root: root.into(), tags, watches: Vec::new(), round: 0, left: 0 }
    }

    /// Whether a completion is of an effect this asked for.
    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.checked_sub(self.tags).is_some_and(|it| it < TAGS)
    }

    /// Whether the round under way is in: every flush it asked for has completed.
    pub fn flushed(&self) -> bool {
        self.left == 0
    }

    /// Watches each directory wanted since the last call.
    pub fn start(&mut self, effects: &mut Vec<Effect>) {
        let len = self.dirs.len() as usize;
        while self.watches.len() < len {
            let at = self.watches.len();
            let file = self.name(self.dirs.dir(at as u32).path());
            let named = file.is_some();
            self.watches.push(Watch {
                file,
                state: State::Idle,
                stat: false,
                flush: None,
                due: false,
                confirmed: false,
                restart: false,
                retry: 0,
                backoff: 1,
            });
            if named {
                self.watch(at, effects);
            }
        }
    }

    /// A round: watches what is wanted and asks again what may now succeed, then flushes every
    /// confirmed watch. Until [`Outside::flushed`], nothing vouches; once it is, every change
    /// made before this was called has been heard. The daemon runs one per request, before
    /// the request's checks.
    pub fn flush(&mut self, effects: &mut Vec<Effect>) {
        self.start(effects);
        self.round += 1;
        self.left = 0;
        self.dirs.settle(false);
        let round = self.round;
        for at in 0..self.watches.len() {
            let watch = &mut self.watches[at];
            watch.due = false;
            if watch.file.is_none() || watch.restart {
                continue;
            }
            let idle = !watch.stat && watch.flush.is_none();
            match watch.state {
                State::Idle if idle && watch.retry <= round => self.watch(at, effects),
                State::Watching if watch.confirmed => {
                    watch.due = true;
                    self.left += 1;
                    if watch.flush.is_none() {
                        watch.flush = Some(round);
                        effects.push(Effect::Flush { watch: self.tag(at, WATCH), tag: self.tag(at, FLUSH) });
                    }
                }
                State::Watching if !watch.stat && watch.retry <= round => self.stat(at, effects),
                _ => {}
            }
        }
        if self.left == 0 {
            self.dirs.settle(true);
        }
    }

    /// Takes a completion of its own.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) {
        let offset = completion.tag.0 - self.tags;
        let (at, kind) = ((offset >> 2) as usize, offset & 3);
        let dirs = self.dirs;
        let dir = dirs.dir(at as u32);
        let round = self.round;
        let watch = &mut self.watches[at];
        match (kind, completion.done) {
            (WATCH, Done::Changed { files, .. }) => {
                dir.changed.fetch_max(dirs.bump(), Ordering::AcqRel);
                if watch.file.as_ref().is_some_and(|it| files.contains(it)) {
                    // Replaced or moved, or may have been: its watch may hear nothing more of
                    // what is at its name.
                    unconfirm(dir, watch);
                    watch.restart = true;
                } else if watch.state == State::Watching
                    && !watch.confirmed
                    && !watch.stat
                    && !watch.restart
                {
                    // Not there when it was asked of, or its stat failed: there now, perhaps.
                    self.stat(at, effects);
                }
            }
            (WATCH, done @ (Done::Unwatched | Done::Failed(_))) => {
                unconfirm(dir, watch);
                watch.state = State::Idle;
                if matches!(done, Done::Failed(_)) && !watch.restart {
                    back_off(watch, round);
                }
            }
            (WATCH, done) => unreachable!("a watch completed with {done:?}"),
            (STAT, done) => {
                watch.stat = false;
                match done {
                    Done::Stat(Some(meta))
                        if meta.dir
                            && watch.state == State::Watching
                            && !watch.confirmed
                            && !watch.restart =>
                    {
                        dir.since.store(dirs.bump(), Ordering::Release);
                        watch.confirmed = true;
                        watch.backoff = 1;
                    }
                    Done::Stat(_) => {}
                    Done::Failed(_) => back_off(watch, round),
                    done => unreachable!("a stat completed with {done:?}"),
                }
            }
            (FLUSH, done) => {
                let asked = watch.flush.take().expect("a flush in flight");
                match done {
                    Done::Flushed => {}
                    Done::Failed(_) => {
                        unconfirm(dir, watch);
                        back_off(watch, round);
                    }
                    done => unreachable!("a flush completed with {done:?}"),
                }
                if watch.due {
                    if asked == round || !watch.confirmed || watch.state != State::Watching {
                        watch.due = false;
                        self.left -= 1;
                        if self.left == 0 {
                            dirs.settle(true);
                        }
                    } else {
                        // Asked in a round before this one: it covers less than this one needs.
                        watch.flush = Some(round);
                        effects.push(Effect::Flush { watch: self.tag(at, WATCH), tag: self.tag(at, FLUSH) });
                    }
                }
            }
            (_, done) => unreachable!("an effect of a watch's completed with {done:?}"),
        }
        self.tend(at, effects);
    }

    /// Moves a watch to be made again on, once nothing of it is in flight.
    fn tend(&mut self, at: usize, effects: &mut Vec<Effect>) {
        let watch = &mut self.watches[at];
        if !watch.restart || watch.stat || watch.flush.is_some() {
            return;
        }
        match watch.state {
            State::Watching => {
                watch.state = State::Ending;
                effects.push(Effect::Unwatch { tag: self.tag(at, WATCH) });
            }
            State::Idle => {
                watch.restart = false;
                self.watch(at, effects);
            }
            State::Ending => {}
        }
    }

    /// Watches the directory `at`, and asks right after for the stat that confirms it.
    fn watch(&mut self, at: usize, effects: &mut Vec<Effect>) {
        let tag = self.tag(at, WATCH);
        let watch = &mut self.watches[at];
        let file = watch.file.clone().expect("a directory the driver names");
        watch.state = State::Watching;
        watch.confirmed = false;
        effects.push(Effect::Watch { dir: file, tag });
        self.stat(at, effects);
    }

    fn stat(&mut self, at: usize, effects: &mut Vec<Effect>) {
        let tag = self.tag(at, STAT);
        let watch = &mut self.watches[at];
        watch.stat = true;
        let file = watch.file.clone().expect("a directory the driver names");
        effects.push(Effect::Stat { file, tag });
    }

    fn tag(&self, at: usize, kind: u64) -> Tag {
        Tag(self.tags + ((at as u64) << 2) + kind)
    }

    /// `path` as the driver names it, when its root can.
    fn name(&self, path: &Path) -> Option<FileName> {
        let rest = path.strip_prefix(&self.root).ok()?.to_str()?;
        if std::path::MAIN_SEPARATOR != '/' && rest.contains(std::path::MAIN_SEPARATOR) {
            return FileName::new(&rest.replace(std::path::MAIN_SEPARATOR, "/"));
        }
        FileName::new(rest)
    }
}

/// Every directory stops vouching once nothing drives it.
impl Drop for Outside<'_> {
    fn drop(&mut self) {
        self.dirs.settle(false);
        for at in 0..self.watches.len() {
            self.dirs.dir(at as u32).since.store(UNCONFIRMED, Ordering::Release);
        }
        self.dirs.driven.store(false, Ordering::Release);
    }
}

fn unconfirm(dir: &Dir, watch: &mut Watch) {
    dir.since.store(UNCONFIRMED, Ordering::Release);
    watch.confirmed = false;
}

fn back_off(watch: &mut Watch, round: u64) {
    watch.retry = round + watch.backoff;
    watch.backoff = (watch.backoff * 2).min(MOST_BACKOFF);
}

#[cfg(test)]
mod tests;
