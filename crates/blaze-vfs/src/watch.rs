//! The disk as one input: a [`Watcher`] watches a directory through [`Effect::Watch`] and
//! turns each change the driver reports into a transaction on one head, by the disk.
//!
//! - Each report becomes a batch: every path in it is looked at ([`Effect::Stat`], then
//!   [`Effect::ReadAll`] for a file), and once all are read the batch is committed, batches in
//!   the order they were told, so a later read of a file wins.
//! - The disk is the truth for the paths it reports, but not over a transaction made since:
//!   a path whose head changed after the report (by anyone but this watcher) is not taken and
//!   is named ([`Watched::Conflict`]).
//! - A file whose bytes are those the head has already makes no op, so a file blaze wrote
//!   itself is not taken back in as an edit.
//! - A watch starts with a scan ([`Effect::List`], asked right after the [`Effect::Watch`] so
//!   no change falls between them): every file on the disk is read, every file the head has
//!   and the disk doesn't is deleted, as one batch, and [`Watched::Scanned`] says the head
//!   holds what the directory did. Changes the system dropped (`lost`) are a rescan
//!   ([`Watched::Lost`]), never a guess of what changed.
//! - Modes: [`Effect::Stat`] says nothing of a file's executable bit or of a symlink, and
//!   [`Effect::ReadAll`] reads through a symlink. So a file the head has keeps its mode, a new
//!   one is [`Mode::Regular`], and a chmod alone is not seen, until the driver reports modes
//!   (v2/requests/vfs.md item 6).

use std::collections::VecDeque;

use blaze_core::{Completion, Done, Effect, FileName, IoError, Meta, Tag};
use blaze_ids::ContentId;
use rustc_hash::FxHashMap;

use crate::{
    Commit, FILE_DOMAIN, Mode, Op, Vfs,
    tree::{NodeId, Tree, ancestors},
};

/// What the head has at a path: its node's content ID and its mode.
type Seen = Option<(ContentId, Mode)>;

fn seen(tree: &Tree, root: NodeId, path: &str) -> Seen {
    tree.entry(root, path).map(|(node, mode)| (tree.id(node), mode))
}

/// How many tags a watcher uses: its watch, its listing, then one per path being read, reused
/// in turn.
const TAGS: u64 = 1 << 32;
const LIST: u64 = 1;
const FIRST_READ: u64 = 2;

/// What a watcher's completion did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Watched {
    /// What the disk changed, committed to the head.
    Committed(Commit),
    /// The head's `path` changed since the disk's change was reported: the head keeps its own.
    Conflict { path: String },
    /// `path` changed and could not be read: the head keeps what it had.
    Unreadable { path: String, error: IoError },
    /// The system dropped changes; a rescan is under way, and ends with [`Watched::Scanned`].
    Lost,
    /// A scan was committed: the head holds what the directory did when it was listed.
    Scanned,
    /// The watch ended: stopped, or failed with the error.
    Ended(Option<IoError>),
}

/// What the disk had at a path.
enum Disk {
    Gone,
    Dir,
    File(Vec<u8>),
    Unreadable,
}

struct Slot {
    /// In the head, relative to the watched directory.
    path: String,
    /// Under the driver's root.
    file: FileName,
    /// What the head had at `path` when the change was reported.
    was: Seen,
    disk: Option<Disk>,
}

struct Batch {
    slots: Vec<Slot>,
    /// A scan's: every file of the directory.
    scan: bool,
    /// A scan's place, taken when the listing was asked: what is told after commits after it.
    /// Its slots come with the listing.
    waiting: bool,
}

pub struct Watcher {
    head: Box<str>,
    dir: FileName,
    tags: u64,
    watching: bool,
    /// A listing is in flight; `again`: another is wanted once it is in.
    listing: bool,
    again: bool,
    /// Batches in the order their changes were told; the first is number `first`.
    batches: VecDeque<Batch>,
    first: u64,
    /// By tag: the batch's number and the slot in it being read.
    reading: FxHashMap<Tag, (u64, usize)>,
    next_tag: u64,
    /// What this watcher's commits left at each path, while batches told before them wait.
    ours: FxHashMap<String, Seen>,
}

impl Watcher {
    /// A watcher of `dir` (under the driver's root) committing to `head`. Tags it asks with are
    /// `tags..tags + 2^32`.
    pub fn new(head: &str, dir: FileName, tags: u64) -> Watcher {
        Watcher {
            head: head.into(),
            dir,
            tags,
            watching: false,
            listing: false,
            again: false,
            batches: VecDeque::new(),
            first: 0,
            reading: FxHashMap::default(),
            next_tag: 0,
            ours: FxHashMap::default(),
        }
    }

    pub fn start(&mut self, effects: &mut Vec<Effect>) {
        assert!(!self.watching, "{} is watched already", self.dir);
        self.watching = true;
        effects.push(Effect::Watch { dir: self.dir.clone(), tag: Tag(self.tags) });
        self.list(effects);
    }

    /// Asks for a listing of the directory, or for one more after the one in flight.
    fn list(&mut self, effects: &mut Vec<Effect>) {
        if self.listing {
            self.again = true;
        } else {
            self.listing = true;
            effects.push(Effect::List { dir: self.dir.clone(), tag: Tag(self.tags + LIST) });
            self.batches.push_back(Batch { slots: Vec::new(), scan: true, waiting: true });
        }
    }

    /// Stops the watch: it ends with [`Watched::Ended`]. Reads in flight still commit.
    pub fn stop(&mut self, effects: &mut Vec<Effect>) {
        if self.watching {
            self.watching = false;
            effects.push(Effect::Unwatch { tag: Tag(self.tags) });
        }
    }

    /// Whether a completion is of an effect this asked for.
    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.checked_sub(self.tags).is_some_and(|it| it < TAGS)
    }

    /// Whether nothing it was told is waiting to be listed, read or committed.
    pub fn idle(&self) -> bool {
        self.batches.is_empty()
    }

    /// Takes a completion of its own; commits what is read in full to the head, and flushes
    /// `vfs` after.
    pub fn complete(
        &mut self,
        vfs: &mut Vfs,
        completion: Completion,
        effects: &mut Vec<Effect>,
    ) -> Vec<Watched> {
        let mut out = Vec::new();
        if completion.tag == Tag(self.tags) {
            match completion.done {
                Done::Changed { lost: true, .. } => {
                    out.push(Watched::Lost);
                    self.list(effects);
                }
                Done::Changed { files, lost: false } => self.told(vfs, files, effects),
                Done::Unwatched => {
                    self.watching = false;
                    out.push(Watched::Ended(None));
                }
                Done::Failed(error) => {
                    self.watching = false;
                    out.push(Watched::Ended(Some(error)));
                }
                done => unreachable!("a watch completed with {done:?}"),
            }
            return out;
        }
        if completion.tag == Tag(self.tags + LIST) {
            self.listing = false;
            match completion.done {
                Done::Listed(listed) => self.scanned(vfs, listed, effects),
                Done::Failed(error) => {
                    let place = self.scan_place();
                    let batch = &mut self.batches[place];
                    (batch.waiting, batch.scan) = (false, false);
                    out.push(Watched::Unreadable { path: String::new(), error });
                    self.again = false;
                    self.stop(effects);
                }
                done => unreachable!("a listing completed with {done:?}"),
            }
            if std::mem::take(&mut self.again) {
                self.list(effects);
            }
            self.drain(vfs, &mut out, effects);
            return out;
        }
        let (batch, at) = self.reading.remove(&completion.tag).expect("a path being read");
        let slot = &mut self.batches[(batch - self.first) as usize].slots[at];
        slot.disk = Some(match completion.done {
            Done::Stat(None) => Disk::Gone,
            Done::Stat(Some(meta)) if meta.dir => Disk::Dir,
            Done::Stat(Some(_)) => {
                effects.push(Effect::ReadAll { file: slot.file.clone(), tag: completion.tag });
                self.reading.insert(completion.tag, (batch, at));
                return out;
            }
            Done::Read(bytes) => Disk::File(bytes),
            Done::Failed(error) => {
                out.push(Watched::Unreadable { path: slot.path.clone(), error });
                Disk::Unreadable
            }
            done => unreachable!("a read of {} completed with {done:?}", slot.file),
        });
        self.drain(vfs, &mut out, effects);
        out
    }

    /// Commits every batch read in full that waits for none told before it.
    fn drain(&mut self, vfs: &mut Vfs, out: &mut Vec<Watched>, effects: &mut Vec<Effect>) {
        while self
            .batches
            .front()
            .is_some_and(|it| !it.waiting && it.slots.iter().all(|it| it.disk.is_some()))
        {
            let batch = self.batches.pop_front().expect("a batch");
            self.first += 1;
            let scan = batch.scan;
            self.commit(vfs, batch, out);
            if scan {
                out.push(Watched::Scanned);
            }
        }
        if self.batches.is_empty() {
            self.ours.clear();
        }
        vfs.flush(effects);
    }

    /// A tag for one path's read.
    fn read_tag(&mut self, number: u64, at: usize) -> Tag {
        let tag = Tag(self.tags + FIRST_READ + self.next_tag);
        self.next_tag = (self.next_tag + 1) % (TAGS - FIRST_READ);
        assert!(!self.reading.contains_key(&tag), "2^32 paths are being read at once");
        self.reading.insert(tag, (number, at));
        tag
    }

    /// Where in `batches` the scan whose listing is in flight waits.
    fn scan_place(&self) -> usize {
        self.batches.iter().position(|it| it.waiting).expect("a scan waits for its listing")
    }

    /// A listing of the directory: its scan's batch, of every file in it, read, and of every
    /// file the head has that it doesn't, gone.
    fn scanned(&mut self, vfs: &Vfs, listed: Vec<(FileName, Meta)>, effects: &mut Vec<Effect>) {
        let head = vfs.heads.get(&*self.head).copied().unwrap_or(vfs.tree.empty());
        let below = format!("{}/", self.dir);
        let place = self.scan_place();
        let number = self.first + place as u64;
        let mut slots = Vec::new();
        let mut on_disk = rustc_hash::FxHashSet::default();
        for (file, meta) in listed {
            // A directory is what the files under it make.
            if meta.dir {
                continue;
            }
            let Some(path) = file.as_str().strip_prefix(&below) else { continue };
            let path = path.to_owned();
            on_disk.insert(path.clone());
            let tag = self.read_tag(number, slots.len());
            effects.push(Effect::ReadAll { file: file.clone(), tag });
            let was = seen(&vfs.tree, head, &path);
            slots.push(Slot { path, file, was, disk: None });
        }
        for (path, node, mode) in vfs.tree.files(head, Mode::Regular, "") {
            if on_disk.contains(&path) {
                continue;
            }
            let file = FileName::new(&format!("{below}{path}")).expect("a head's path is a name");
            let was = Some((vfs.tree.id(node), mode));
            slots.push(Slot { path, file, was, disk: Some(Disk::Gone) });
        }
        let batch = &mut self.batches[place];
        batch.slots = slots;
        batch.waiting = false;
    }

    /// A report of changes: a batch of the paths under the watched directory, each looked at.
    fn told(&mut self, vfs: &Vfs, files: Vec<FileName>, effects: &mut Vec<Effect>) {
        let head = vfs.heads.get(&*self.head).copied().unwrap_or(vfs.tree.empty());
        let below = format!("{}/", self.dir);
        let number = self.first + self.batches.len() as u64;
        let mut slots = Vec::new();
        for file in files {
            let Some(path) = file.as_str().strip_prefix(&below) else { continue };
            let path = path.to_owned();
            let was = seen(&vfs.tree, head, &path);
            let tag = self.read_tag(number, slots.len());
            effects.push(Effect::Stat { file: file.clone(), tag });
            slots.push(Slot { path, file, was, disk: None });
        }
        if !slots.is_empty() {
            self.batches.push_back(Batch { slots, scan: false, waiting: false });
        }
    }

    fn commit(&mut self, vfs: &mut Vfs, batch: Batch, out: &mut Vec<Watched>) {
        let base = vfs.heads.get(&*self.head).copied().unwrap_or(vfs.tree.empty());
        let tree = &vfs.tree;
        let mut deletes: Vec<String> = Vec::new();
        let mut writes: Vec<(String, Vec<u8>, Mode)> = Vec::new();
        let mut taken = Vec::new();
        for slot in batch.slots {
            let entry = tree.entry(base, &slot.path);
            let node = entry.map(|it| it.0);
            let now = entry.map(|(node, mode)| (tree.id(node), mode));
            if now != slot.was && !self.made(&slot.path, now) {
                out.push(Watched::Conflict { path: slot.path });
                continue;
            }
            let is_file = node.is_some_and(|it| !tree.is_dir(it));
            match slot.disk.expect("a read path") {
                Disk::Unreadable => continue,
                Disk::Gone if node.is_some() => deletes.push(slot.path.clone()),
                Disk::Gone => {}
                Disk::Dir if is_file => deletes.push(slot.path.clone()),
                Disk::Dir => {}
                Disk::File(bytes) => {
                    // A file there means every directory above it is one.
                    let mut above: Vec<&str> = ancestors(&slot.path)
                        .filter(|up| tree.lookup(base, up).is_some_and(|it| !tree.is_dir(it)))
                        .collect();
                    above.reverse();
                    deletes.extend(above.into_iter().map(str::to_owned));
                    if node.is_some() && !is_file {
                        deletes.push(slot.path.clone());
                    }
                    let mode = entry.filter(|_| is_file).map_or(Mode::Regular, |it| it.1);
                    if now != Some((ContentId::of(FILE_DOMAIN, &bytes), mode)) {
                        writes.push((slot.path.clone(), bytes, mode));
                    }
                }
            }
            taken.push(slot.path);
        }
        deletes.sort_unstable();
        deletes.dedup();
        // A path under one deleted goes with it.
        let deleted = |path: &str| deletes.binary_search_by(|it| it.as_str().cmp(path)).is_ok();
        let deletes: Vec<String> =
            deletes.iter().filter(|path| !ancestors(path).any(deleted)).cloned().collect();
        let ops: Vec<Op> = deletes
            .into_iter()
            .map(|path| Op::Delete { path })
            .chain(writes.into_iter().map(|(path, bytes, mode)| Op::Write { path, bytes, mode }))
            .collect();
        if ops.is_empty() {
            return;
        }
        let base = vfs.tree.id(base);
        let commit = vfs.commit(&self.head, base, &ops).unwrap_or_else(|error| {
            panic!("the disk's changes to {} do not apply to its head: {error}", self.dir)
        });
        let root = vfs.root(commit.root).expect("the root just made");
        let paths = ops.iter().map(|op| match op {
            Op::Delete { path } | Op::Write { path, .. } => path,
            Op::Edit { .. } | Op::Rename { .. } => unreachable!("the disk makes no {op:?}"),
        });
        for path in taken.iter().chain(paths) {
            self.ours.insert(path.clone(), seen(&vfs.tree, root, path));
        }
        out.push(Watched::Committed(commit));
    }

    /// Whether this watcher's commits left `path` as `now`: it put that there, or `path` is gone
    /// because it replaced or deleted something above it.
    fn made(&self, path: &str, now: Seen) -> bool {
        self.ours.get(path) == Some(&now)
            || (now.is_none() && ancestors(path).any(|up| self.ours.contains_key(up)))
    }
}
