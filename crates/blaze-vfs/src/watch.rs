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
//! - Changes the system dropped (`lost`) stop the watch ([`Watched::Lost`]): the head no longer
//!   follows the disk, and nothing guesses what changed.
//! - Modes: [`Effect::Stat`] says nothing of a file's executable bit or of a symlink, and
//!   [`Effect::ReadAll`] reads through a symlink. So a file the head has keeps its mode, a new
//!   one is [`Mode::Regular`], and a chmod alone is not seen, until the driver reports modes
//!   (v2/requests/vfs.md item 6).

use std::collections::VecDeque;

use blaze_core::{Completion, Done, Effect, FileName, IoError, Tag};
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

/// How many tags a watcher uses: its watch, then one per path being read, reused in turn.
const TAGS: u64 = 1 << 32;

/// What a watcher's completion did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Watched {
    /// What the disk changed, committed to the head.
    Committed(Commit),
    /// The head's `path` changed since the disk's change was reported: the head keeps its own.
    Conflict { path: String },
    /// `path` changed and could not be read: the head keeps what it had.
    Unreadable { path: String, error: IoError },
    /// The system dropped changes, so the head no longer follows the disk; the watch is being
    /// stopped.
    Lost,
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
}

pub struct Watcher {
    head: Box<str>,
    dir: FileName,
    tags: u64,
    watching: bool,
    lost: bool,
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
    /// A watcher of `dir` (under the driver's root) committing to `head`, which must hold what
    /// `dir` holds when the watch starts. Tags it asks with are `tags..tags + 2^32`.
    pub fn new(head: &str, dir: FileName, tags: u64) -> Watcher {
        Watcher {
            head: head.into(),
            dir,
            tags,
            watching: false,
            lost: false,
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
        self.lost = false;
        effects.push(Effect::Watch { dir: self.dir.clone(), tag: Tag(self.tags) });
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

    /// Whether nothing it was told is waiting to be read or committed.
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
                Done::Changed { .. } if self.lost => {}
                Done::Changed { lost: true, .. } => {
                    self.lost = true;
                    self.stop(effects);
                    out.push(Watched::Lost);
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
        while self.batches.front().is_some_and(|it| it.slots.iter().all(|it| it.disk.is_some())) {
            let batch = self.batches.pop_front().expect("a batch");
            self.first += 1;
            self.commit(vfs, batch, &mut out);
        }
        if self.batches.is_empty() {
            self.ours.clear();
        }
        vfs.flush(effects);
        out
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
            let tag = Tag(self.tags + 1 + self.next_tag);
            self.next_tag = (self.next_tag + 1) % (TAGS - 1);
            assert!(!self.reading.contains_key(&tag), "2^32 paths are being read at once");
            self.reading.insert(tag, (number, slots.len()));
            effects.push(Effect::Stat { file: file.clone(), tag });
            slots.push(Slot { path, file, was, disk: None });
        }
        if !slots.is_empty() {
            self.batches.push_back(Batch { slots });
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
