//! The disk as one input: a [`Watcher`] follows a directory through the system's journal of
//! changes ([`Effect::Journal`]) and turns what changed into transactions on one head, by the
//! disk (docs/v2/plan/design/restore.md, docs/v2/plan/lanes/frontend/journal.md).
//!
//! - **Start**: the journal is asked for what changed since the cursor the head's [`Disk`] saved
//!   (FSEvents' event ID, NTFS's USN). With no cursor (a first start, or Linux, whose journal
//!   does not outlive a restart) the whole directory is looked at again ([`Watcher::rescan`]),
//!   which with a saved column is a sweep of stamps, not a read of every file.
//! - **Every look is stamps first**: the paths of a batch are stamped together in one
//!   [`Effect::Stamp`] (io_uring's batched `statx` on Linux); a path whose stamp is the one the
//!   column saved, and which the head has, is unchanged and never read or hashed. The rest are
//!   read ([`Effect::ReadAll`]), after their stamp was taken, so a change during the read moves
//!   the stamp and is seen again.
//! - **A rescan of a subtree** (a cold start, a Linux start, or a [`blaze_core::Lost`] the journal
//!   names) stamps every path the column and the head have under it, lists
//!   ([`Effect::Entries`]) only the directories whose stamp moved, and every directory new to
//!   it, and reads only files that are new or moved. Nothing outside the subtree is looked at.
//! - A batch commits once every look in it is in, batches in the order they were told, so a
//!   later read of a file wins; its stamps and the journal's cursor after it are saved in the
//!   head's [`Disk`] then, so a cursor is never past a change not committed.
//! - The disk is the truth for the paths it reports, but not over a transaction made since: a
//!   path whose head changed after the report (by anyone but this watcher) is not taken and is
//!   named ([`Watched::Conflict`]). A file whose bytes are those the head has already makes no op.
//! - Modes: a file the head has keeps its mode, a new one is [`Mode::Regular`] (the stamp's
//!   mode bits are saved for when the driver reports modes, v2/requests/vfs.md item 6).

use std::collections::VecDeque;

use blaze_core::{Completion, Cursor, Done, Effect, FileName, IoError, Names, Stamp, Tag};
use blaze_ids::ContentId;
use rustc_hash::FxHashMap;

use crate::{
    Commit, Mode, Op, Vfs,
    disk::Disk,
    tree::{NodeId, Tree, ancestors},
};

/// What the head has at a path: its node's content ID and its mode.
type Seen = Option<(ContentId, Mode)>;

fn seen(tree: &Tree, root: NodeId, path: &str) -> Seen {
    tree.entry(root, path).map(|(node, mode)| (tree.id(node), mode))
}

/// How many tags a watcher uses: its journal, then one per stamp, listing or read in flight.
const TAGS: u64 = 1 << 32;
const FIRST: u64 = 1;

/// What a watcher's completion did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Watched {
    /// What the disk changed, committed to the head.
    Committed(Commit),
    /// The head's `path` changed since the disk's change was reported: the head keeps its own.
    Conflict { path: String },
    /// `path` changed and could not be read: the head keeps what it had. `""` is the watched
    /// directory itself, which ends the watch.
    Unreadable { path: String, error: IoError },
    /// The journal could not name the changes under `path` (`""` for all of it), for `why`: that
    /// subtree is looked at again, and only it; the look ends with [`Watched::Scanned`].
    Lost { path: String, why: blaze_core::Lost },
    /// The head holds what the directory did: a look at a subtree, or everything the journal
    /// told up to where it caught up, is committed.
    Scanned,
    /// The watch ended: stopped, or failed with the error.
    Ended(Option<IoError>),
}

/// What the disk had at a path.
enum Look {
    Gone,
    Dir,
    File(Vec<u8>),
    /// Its stamp is the one saved and the head has it: not read.
    Same,
    Unreadable,
}

struct Slot {
    /// In the head, relative to the watched directory (`""` for the directory itself).
    path: String,
    /// Under the driver's root.
    file: FileName,
    /// What the head had at `path` when the change was reported.
    was: Seen,
    /// What the column saved of it, and what it is now, taken before any read.
    saved: Option<Stamp>,
    stamp: Option<Stamp>,
    look: Option<Look>,
}

struct Batch {
    slots: Vec<Slot>,
    /// Each slot by name, once a listing may add names to it.
    index: FxHashMap<Box<str>, usize>,
    /// Stamps and listings in flight: it commits once none is and every slot is looked at.
    open: u32,
    /// The subtree a rescan looks at, relative to the watched directory.
    rescan: Option<String>,
    /// The journal's place once this is committed, and whether it then has told everything
    /// from before the watch.
    cursor: Option<Cursor>,
    caught_up: bool,
}

enum Pending {
    Stamp(u64),
    Entries(u64, FileName),
    Read(u64, usize),
}

pub struct Watcher {
    head: Box<str>,
    dir: FileName,
    /// `dir` and a `/`: what every name under it starts with.
    below: String,
    tags: u64,
    watching: bool,
    caught_up: bool,
    /// Batches in the order their changes were told; the first is number `first`.
    batches: VecDeque<Batch>,
    first: u64,
    pending: FxHashMap<Tag, Pending>,
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
            below: format!("{dir}/"),
            dir,
            tags,
            watching: false,
            caught_up: false,
            batches: VecDeque::new(),
            first: 0,
            pending: FxHashMap::default(),
            next_tag: 0,
            ours: FxHashMap::default(),
        }
    }

    /// Starts following the directory from where `vfs` saved this head's disk, or from scratch.
    pub fn start(&mut self, vfs: &Vfs, effects: &mut Vec<Effect>) {
        assert!(!self.watching, "{} is watched already", self.dir);
        self.watching = true;
        let since = vfs
            .disks
            .get(&self.head)
            .filter(|disk| disk.dir == self.dir)
            .map_or(Cursor::None, |disk| disk.cursor);
        effects.push(Effect::Journal { dir: self.dir.clone(), since, tag: Tag(self.tags) });
        if since == Cursor::None {
            self.rescan(vfs, self.dir.clone(), effects);
        }
    }

    /// Stops the watch: it ends with [`Watched::Ended`]. Looks in flight still commit.
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

    /// Whether nothing it was told is waiting to be looked at or committed.
    pub fn idle(&self) -> bool {
        self.batches.is_empty()
    }

    fn rel<'a>(&self, file: &'a FileName) -> Option<&'a str> {
        match file.as_str().strip_prefix(&self.below) {
            Some(rest) => Some(rest),
            None => (*file == self.dir).then_some(""),
        }
    }

    fn head_root(&self, vfs: &Vfs) -> NodeId {
        vfs.heads.get(&*self.head).copied().unwrap_or(vfs.tree.empty())
    }

    fn tag(&mut self, pending: Pending) -> Tag {
        let tag = Tag(self.tags + FIRST + self.next_tag);
        self.next_tag = (self.next_tag + 1) % (TAGS - FIRST);
        assert!(self.pending.insert(tag, pending).is_none(), "2^32 looks in flight at once");
        tag
    }

    fn batch(&mut self, number: u64) -> Option<&mut Batch> {
        number.checked_sub(self.first).and_then(|it| self.batches.get_mut(it as usize))
    }

    /// Takes a completion of its own; commits what is looked at in full to the head, and
    /// flushes `vfs` after.
    pub fn complete(
        &mut self,
        vfs: &mut Vfs,
        completion: Completion,
        effects: &mut Vec<Effect>,
    ) -> Vec<Watched> {
        let mut out = Vec::new();
        if completion.tag == Tag(self.tags) {
            match completion.done {
                Done::Journaled(batch) => self.journaled(vfs, *batch, effects, &mut out),
                Done::Unwatched => {
                    self.watching = false;
                    out.push(Watched::Ended(None));
                }
                Done::Failed(error) => {
                    self.watching = false;
                    out.push(Watched::Ended(Some(error)));
                }
                done => unreachable!("a journal completed with {done:?}"),
            }
            self.drain(vfs, &mut out, effects);
            return out;
        }
        let Some(pending) = self.pending.remove(&completion.tag) else {
            unreachable!("a completion {:?} of nothing asked", completion.tag)
        };
        match (pending, completion.done) {
            (Pending::Stamp(number), Done::Stamped(stamps)) => {
                self.stamped(number, stamps, effects, &mut out);
            }
            (Pending::Entries(number, _), Done::Entries(entries)) => {
                self.listed(vfs, number, entries, effects);
            }
            (Pending::Read(number, at), done) => {
                if let Some(slot) = self.batch(number).and_then(|it| it.slots.get_mut(at)) {
                    slot.look = Some(match done {
                        Done::Read(bytes) => Look::File(bytes),
                        Done::Failed(error) => {
                            out.push(Watched::Unreadable { path: slot.path.clone(), error });
                            Look::Unreadable
                        }
                        done => unreachable!("a read of {} completed with {done:?}", slot.file),
                    });
                }
            }
            (Pending::Stamp(number), Done::Failed(error)) => {
                let Some(batch) = self.batch(number) else { unreachable!("stamps of no batch") };
                batch.open -= 1;
                let path = batch.rescan.clone().unwrap_or_default();
                for slot in batch.slots.iter_mut().filter(|it| it.look.is_none()) {
                    slot.look = Some(Look::Unreadable);
                }
                if batch.rescan.as_deref() == Some("") {
                    self.stop(effects);
                }
                out.push(Watched::Unreadable { path, error });
            }
            (Pending::Entries(number, dir), Done::Failed(error)) => {
                let Some(batch) = self.batch(number) else { unreachable!("a listing of no batch") };
                batch.open -= 1;
                let path = self.rel(&dir).unwrap_or_default().to_owned();
                if path.is_empty() {
                    self.stop(effects);
                }
                out.push(Watched::Unreadable { path, error });
            }
            (_, done) => unreachable!("a look completed with {done:?}"),
        }
        self.drain(vfs, &mut out, effects);
        out
    }

    /// A batch of the journal's: what it names is looked at, each subtree it lost is looked at
    /// again, and its cursor is saved once they and everything told before are committed.
    fn journaled(
        &mut self,
        vfs: &mut Vfs,
        told: blaze_core::Journaled,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Watched>,
    ) {
        let blaze_core::Journaled { files, lost, cursor, caught_up } = told;
        self.told(vfs, files, effects);
        for (under, why) in lost {
            let Some(path) = self.rel(&under).map(str::to_owned) else { continue };
            out.push(Watched::Lost { path, why });
            self.rescan(vfs, under, effects);
        }
        let caught_up = caught_up && !std::mem::replace(&mut self.caught_up, caught_up || self.caught_up);
        match self.batches.back_mut() {
            Some(last) => {
                last.cursor = Some(cursor);
                last.caught_up |= caught_up;
            }
            None => {
                vfs.disks.of(&self.head, &self.dir).cursor = cursor;
                if caught_up {
                    out.push(Watched::Scanned);
                }
            }
        }
    }

    /// The names the journal told: a batch of them, stamped together.
    fn told(&mut self, vfs: &Vfs, files: Vec<FileName>, effects: &mut Vec<Effect>) {
        let head = self.head_root(vfs);
        let disk = vfs.disks.get(&self.head).filter(|it| it.dir == self.dir);
        let mut slots = Vec::with_capacity(files.len());
        for file in files {
            let Some(path) = self.rel(&file).filter(|it| !it.is_empty()).map(str::to_owned) else {
                continue;
            };
            let saved = disk.and_then(|it| it.get(file.as_str()));
            let was = seen(&vfs.tree, head, &path);
            slots.push(Slot { path, file, was, saved, stamp: None, look: None });
        }
        if !slots.is_empty() {
            self.ask_stamps(slots, None, effects);
        }
    }

    /// Looks at the subtree `under` again: every path the column and the head have under it is
    /// stamped, and the directories whose stamp moved are listed.
    pub fn rescan(&mut self, vfs: &Vfs, under: FileName, effects: &mut Vec<Effect>) {
        let Some(root) = self.rel(&under).map(str::to_owned) else { return };
        let head = self.head_root(vfs);
        let mut names = Names::default();
        let mut saved = Vec::new();
        if let Some(disk) = vfs.disks.get(&self.head).filter(|it| it.dir == self.dir) {
            disk.sweep(under.as_str(), &mut names, &mut saved);
        }
        let mut slots: Vec<Slot> = Vec::with_capacity(names.len() + 1);
        let mut index = FxHashMap::default();
        for (at, name) in names.iter().enumerate() {
            let file = FileName::new(name).expect("a column's name is a name");
            let path = self.rel(&file).expect("a column's name is under its directory").to_owned();
            let was = seen(&vfs.tree, head, &path);
            index.insert(Box::<str>::from(name), slots.len());
            slots.push(Slot { path, file, was, saved: Some(saved[at]), stamp: None, look: None });
        }
        let mut add = |path: String, was: Seen, index: &mut FxHashMap<Box<str>, usize>| {
            let name = if path.is_empty() { self.dir.to_string() } else { format!("{}{path}", self.below) };
            if index.contains_key(name.as_str()) {
                return;
            }
            let file = FileName::new(&name).expect("a head's path is a name");
            index.insert(name.into(), slots.len());
            slots.push(Slot { path, file, was, saved: None, stamp: None, look: None });
        };
        add(root.clone(), seen(&vfs.tree, head, &root), &mut index);
        if let Some(node) = vfs.tree.lookup(head, &root) {
            for (path, node, mode) in vfs.tree.files(node, Mode::Regular, &root) {
                add(path, Some((vfs.tree.id(node), mode)), &mut index);
            }
        }
        let number = self.ask_stamps(slots, Some(root), effects);
        self.batch(number).expect("just made").index = index;
    }

    /// A batch of `slots`, and the stamp of each asked for in one effect. Returns its number.
    fn ask_stamps(&mut self, slots: Vec<Slot>, rescan: Option<String>, effects: &mut Vec<Effect>) -> u64 {
        let number = self.first + self.batches.len() as u64;
        let mut names = Names::with_capacity(slots.len(), slots.len() * 32);
        for slot in &slots {
            names.push(&slot.file);
        }
        let tag = self.tag(Pending::Stamp(number));
        effects.push(Effect::Stamp { files: names, tag });
        self.batches.push_back(Batch {
            slots,
            index: FxHashMap::default(),
            open: 1,
            rescan,
            cursor: None,
            caught_up: false,
        });
        number
    }

    /// The stamps of a batch: what is unchanged is settled, what moved is read, and each
    /// directory that is new, replaced, or (in a rescan) moved is listed.
    fn stamped(&mut self, number: u64, stamps: Vec<Stamp>, effects: &mut Vec<Effect>, out: &mut Vec<Watched>) {
        let mut reads = Vec::new();
        let mut lists = Vec::new();
        let mut stop = false;
        {
            let Some(batch) = self.batch(number) else { unreachable!("stamps of no batch") };
            assert_eq!(stamps.len(), batch.slots.len(), "a stamp per name");
            let rescan = batch.rescan.is_some();
            batch.open -= 1;
            for (at, (slot, stamp)) in batch.slots.iter_mut().zip(stamps).enumerate() {
                slot.stamp = Some(stamp);
                if stamp.is_gone() {
                    if slot.path.is_empty() {
                        out.push(Watched::Unreadable {
                            path: String::new(),
                            error: IoError(format!("{}: no such directory", slot.file)),
                        });
                        slot.look = Some(Look::Unreadable);
                        stop = true;
                    } else {
                        slot.look = Some(Look::Gone);
                    }
                } else if stamp.is_dir() {
                    slot.look = Some(Look::Dir);
                    let list = match slot.saved {
                        None => true,
                        Some(saved) if rescan => saved.moved(&stamp),
                        Some(saved) => saved.ino != stamp.ino,
                    };
                    if list {
                        lists.push(slot.file.clone());
                    }
                } else if slot.saved == Some(stamp) && slot.was.is_some() {
                    slot.look = Some(Look::Same);
                } else {
                    reads.push((at, slot.file.clone()));
                }
            }
            if !lists.is_empty() && batch.index.is_empty() {
                batch.index =
                    batch.slots.iter().enumerate().map(|(at, it)| (it.file.as_str().into(), at)).collect();
            }
            batch.open += lists.len() as u32;
        }
        for (at, file) in reads {
            let tag = self.tag(Pending::Read(number, at));
            effects.push(Effect::ReadAll { file, tag });
        }
        for dir in lists {
            let tag = self.tag(Pending::Entries(number, dir.clone()));
            effects.push(Effect::Entries { dir, tag });
        }
        if stop {
            self.stop(effects);
        }
    }

    /// A listing of a directory of a batch: each name the batch has not looked at is new to it,
    /// read if a file and listed if a directory.
    fn listed(&mut self, vfs: &Vfs, number: u64, entries: Vec<(FileName, Stamp)>, effects: &mut Vec<Effect>) {
        let head = self.head_root(vfs);
        let below = self.below.len();
        let mut asks = Vec::new();
        {
            let Some(batch) = self.batch(number) else { unreachable!("a listing of no batch") };
            batch.open -= 1;
            for (file, stamp) in entries {
                if batch.index.contains_key(file.as_str()) || file.as_str().len() <= below {
                    continue;
                }
                let path = file.as_str()[below..].to_owned();
                let at = batch.slots.len();
                batch.index.insert(file.as_str().into(), at);
                let was = seen(&vfs.tree, head, &path);
                let look = if stamp.is_gone() {
                    Some(Look::Gone)
                } else if stamp.is_dir() {
                    asks.push((None, file.clone()));
                    batch.open += 1;
                    Some(Look::Dir)
                } else {
                    asks.push((Some(at), file.clone()));
                    None
                };
                batch.slots.push(Slot { path, file, was, saved: None, stamp: Some(stamp), look });
            }
        }
        for (at, file) in asks {
            match at {
                Some(at) => {
                    let tag = self.tag(Pending::Read(number, at));
                    effects.push(Effect::ReadAll { file, tag });
                }
                None => {
                    let tag = self.tag(Pending::Entries(number, file.clone()));
                    effects.push(Effect::Entries { dir: file, tag });
                }
            }
        }
    }

    /// Commits every batch looked at in full that waits for none told before it.
    fn drain(&mut self, vfs: &mut Vfs, out: &mut Vec<Watched>, effects: &mut Vec<Effect>) {
        while self
            .batches
            .front()
            .is_some_and(|it| it.open == 0 && it.slots.iter().all(|it| it.look.is_some()))
        {
            let batch = self.batches.pop_front().expect("a batch");
            self.first += 1;
            let scanned = batch.rescan.is_some() || batch.caught_up;
            self.commit(vfs, batch, out);
            if scanned {
                out.push(Watched::Scanned);
            }
        }
        if self.batches.is_empty() {
            self.ours.clear();
        }
        vfs.flush(effects);
    }

    fn commit(&mut self, vfs: &mut Vfs, batch: Batch, out: &mut Vec<Watched>) {
        let disk = vfs.disks.of(&self.head, &self.dir);
        save(disk, &batch);
        let base = vfs.heads.get(&*self.head).copied().unwrap_or(vfs.tree.empty());
        let tree = &vfs.tree;
        let mut deletes: Vec<String> = Vec::new();
        let mut writes: Vec<(String, Vec<u8>, Mode)> = Vec::new();
        let mut taken = Vec::new();
        for slot in batch.slots {
            if slot.path.is_empty() || matches!(slot.look, Some(Look::Same | Look::Unreadable)) {
                continue;
            }
            let entry = tree.entry(base, &slot.path);
            let node = entry.map(|it| it.0);
            let now = entry.map(|(node, mode)| (tree.id(node), mode));
            if now != slot.was && !self.made(&slot.path, now) {
                out.push(Watched::Conflict { path: slot.path });
                continue;
            }
            let is_file = node.is_some_and(|it| !tree.is_dir(it));
            match slot.look.expect("a looked-at path") {
                Look::Same | Look::Unreadable => unreachable!("skipped above"),
                Look::Gone if node.is_some() => deletes.push(slot.path.clone()),
                Look::Gone => {}
                Look::Dir if is_file => deletes.push(slot.path.clone()),
                Look::Dir => {}
                Look::File(bytes) => {
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
                    // Compared in place, not hashed: the commit hashes what it writes, once.
                    let same = node
                        .filter(|_| is_file)
                        .and_then(|it| tree.bytes(it))
                        .is_some_and(|it| **it == *bytes);
                    if !same {
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

/// What a committed batch saw goes into the column: each path's stamp (a conflict's too: the
/// disk was seen and the head chose), gone paths with everything under them, and the journal's
/// cursor after it. An unreadable path keeps what it had, so it is looked at again.
fn save(disk: &mut Disk, batch: &Batch) {
    for slot in &batch.slots {
        let Some(stamp) = slot.stamp else { continue };
        match slot.look {
            Some(Look::Unreadable) | None => {}
            Some(Look::Gone) => disk.remove_under(slot.file.as_str()),
            Some(Look::Same) => {}
            Some(Look::Dir | Look::File(_)) => disk.set(slot.file.as_str(), stamp),
        }
    }
    if let Some(cursor) = batch.cursor {
        disk.cursor = cursor;
    }
}

#[cfg(test)]
mod tests;
