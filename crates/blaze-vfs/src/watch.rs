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
//! - **A rescan of a subtree** (a cold start, a Linux start, or a [`Lost`] the journal names)
//!   stamps every path the column and the head have under it, lists ([`Effect::Entries`]) only
//!   the directories whose stamp moved, and every directory new to it, and reads only files
//!   that are new or moved. Nothing outside the subtree is looked at. [`Lost::Unnamed`] (names
//!   in one directory changed, not which) lists that directory alone and diffs it.
//! - **A move is not a read**: a name new to a batch whose stamp is that of a file the batch
//!   found gone (the same inode, size, mtime and type) is that file moved, and the head's own
//!   node goes there ([`Op::Rename`] where the source goes in the same commit, else its bytes
//!   from the tree). A directory renamed while blaze was down reads none of its files.
//! - A batch commits once every look in it is in, batches in the order they were told, so a
//!   later read of a file wins; its stamps and the journal's cursor after it are saved in the
//!   head's [`Disk`] then, so a cursor is never past a change not committed.
//! - The disk is the truth for the paths it reports, but not over a transaction made since: a
//!   path whose head changed after the report (by anyone but this watcher) is not taken and is
//!   named ([`Watched::Conflict`]). A file whose bytes are those the head has already makes no op.
//! - Modes: a file the head has keeps its mode, a new one is [`Mode::Regular`] (the stamp's
//!   mode bits are saved for when the driver reports modes, v2/requests/vfs.md item 6).

use std::collections::VecDeque;

use blaze_core::{
    Completion, Cursor, Done, Effect, FileName, IoError, Lost, Names, S_IFMT, Stamp, Tag,
};
use blaze_ids::ContentId;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    Commit, Mode, Op, Vfs,
    disk::Disk,
    tree::{NodeId, Tree, ancestors, op_paths},
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
    /// subtree is looked at again, and only it ([`Lost::Unnamed`]: only the directory's own
    /// names); the look ends with [`Watched::Scanned`].
    Lost { path: String, why: Lost },
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
    /// A file new to its path, with the stamp of the batch's gone slot at this index: that
    /// file moved here. Not read.
    Moved(usize),
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
    /// A rescan's sweep: the column's names under its subtree, sorted, with what it saved of
    /// each. They are stamped first, compared in one pass, and only a name whose stamp moved
    /// (or that `forced` names) becomes a slot: an unchanged one costs no allocation and no
    /// look in the tree.
    names: Names,
    saved: Vec<Stamp>,
    /// Swept names the head changed since the column held all of it, ascending.
    forced: Vec<u32>,
    slots: Vec<Slot>,
    /// Each slot by name, once a listing may add names to it (the swept names are found in
    /// `names`).
    index: FxHashMap<Box<str>, usize>,
    /// Stamps and listings in flight: it commits once none is and every slot is looked at.
    open: u32,
    /// The subtree a rescan looks at, relative to the watched directory.
    rescan: Option<String>,
    /// The directory a [`Lost::Unnamed`] lists alone: its slots are what was in it.
    relist: Option<FileName>,
    /// Each gone file the head had, by its inode: a name new to the batch with its stamp is it.
    gone: FxHashMap<u64, usize>,
    /// The journal's place once this is committed, and whether it then has told everything
    /// from before the watch.
    cursor: Option<Cursor>,
    caught_up: bool,
}

impl Batch {
    fn new(slots: Vec<Slot>, rescan: Option<String>, relist: Option<FileName>) -> Batch {
        Batch {
            names: Names::default(),
            saved: Vec::new(),
            forced: Vec::new(),
            slots,
            index: FxHashMap::default(),
            open: 1,
            rescan,
            relist,
            gone: FxHashMap::default(),
            cursor: None,
            caught_up: false,
        }
    }

    /// Where `name` is among the swept names, or where it would go.
    fn swept(&self, name: &str) -> Result<usize, usize> {
        let (mut low, mut high) = (0, self.names.len());
        while low < high {
            let mid = (low + high) / 2;
            match self.names.get(mid).cmp(name) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(low)
    }

    /// Whether the batch looks at `name` already.
    fn known(&self, name: &str) -> bool {
        self.index.contains_key(name) || self.swept(name).is_ok()
    }

    fn indexed(&mut self) {
        if self.index.is_empty() {
            self.index =
                self.slots.iter().enumerate().map(|(at, it)| (it.file.as_str().into(), at)).collect();
        }
    }

    /// Takes in what is gone: what the column had under each gone directory is gone too (a
    /// slot each), and each file the head had that left its path (gone, or another inode is
    /// there now) is kept by its inode for [`Batch::moved`].
    fn went(&mut self, disk: Option<&Disk>, tree: &Tree, head: NodeId, below: usize) {
        let gone_dir = |it: &Slot| matches!(it.look, Some(Look::Gone)) && it.saved.is_some_and(|it| it.is_dir());
        if let Some(disk) = disk
            && self.slots.iter().any(gone_dir)
        {
            self.indexed();
            let dirs: Vec<FileName> =
                self.slots.iter().filter(|it| gone_dir(it)).map(|it| it.file.clone()).collect();
            for dir in dirs {
                disk.under(dir.as_str(), |name, saved| {
                    if name == dir.as_str() || self.known(name) {
                        return;
                    }
                    let file = FileName::new(name).expect("a column's name is a name");
                    let path = name[below..].to_owned();
                    let was = seen(tree, head, &path);
                    self.index.insert(name.into(), self.slots.len());
                    let look = Some(Look::Gone);
                    self.slots.push(Slot { path, file, was, saved: Some(saved), stamp: Some(Stamp::GONE), look });
                });
            }
        }
        for (at, slot) in self.slots.iter().enumerate() {
            let (Some(saved), Some(_)) = (slot.saved, slot.was) else { continue };
            let left = match slot.look {
                Some(Look::Gone) => true,
                _ => slot.stamp.is_some_and(|now| !now.is_gone() && now.ino != saved.ino),
            };
            if left && !saved.is_dir() {
                self.gone.entry(saved.ino).or_insert(at);
            }
        }
    }

    /// The gone slot a file new to the batch, with `stamp`, moved from.
    fn moved(&self, stamp: &Stamp) -> Option<usize> {
        if stamp.is_gone() || stamp.is_unknown() || stamp.is_dir() {
            return None;
        }
        let &at = self.gone.get(&stamp.ino)?;
        let saved = self.slots[at].saved?;
        let same = saved.size == stamp.size
            && saved.mtime_ns == stamp.mtime_ns
            && saved.mode & S_IFMT == stamp.mode & S_IFMT;
        same.then_some(at)
    }
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
    /// What the journal told of before the watch, held until it has told all of it.
    held: (Vec<FileName>, Vec<(FileName, Lost)>),
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
            held: (Vec::new(), Vec::new()),
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
        self.batches.is_empty() && self.held.0.is_empty() && self.held.1.is_empty()
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

    fn disk<'a>(&self, vfs: &'a Vfs) -> Option<&'a Disk> {
        vfs.disks.get(&self.head).filter(|it| it.dir == self.dir)
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

    fn push(&mut self, batch: Batch) -> u64 {
        let number = self.first + self.batches.len() as u64;
        self.batches.push_back(batch);
        number
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
                self.stamped(vfs, number, stamps, effects, &mut out);
            }
            (Pending::Entries(number, dir), Done::Entries(entries)) => {
                self.listed(vfs, number, &dir, entries, effects);
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
                if batch.relist.as_ref() == Some(&dir) {
                    for slot in batch.slots.iter_mut().filter(|it| it.look.is_none()) {
                        slot.look = Some(Look::Unreadable);
                    }
                }
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
    /// again, and its cursor is saved once they and everything told before are committed. What
    /// changed before the watch is held until the journal has told all of it, then looked at
    /// as one batch and committed as one transaction: a rename told across two of the
    /// journal's batches is still one move.
    fn journaled(
        &mut self,
        vfs: &mut Vfs,
        told: blaze_core::Journaled,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Watched>,
    ) {
        let blaze_core::Journaled { mut files, mut lost, cursor, caught_up } = told;
        if !self.caught_up {
            self.held.0.append(&mut files);
            self.held.1.append(&mut lost);
            if !caught_up {
                return;
            }
            (files, lost) = std::mem::take(&mut self.held);
            files.sort_unstable();
            files.dedup();
        }
        self.told(vfs, files, effects);
        for (under, why) in lost {
            let Some(path) = self.rel(&under).map(str::to_owned) else { continue };
            out.push(Watched::Lost { path, why });
            match why {
                Lost::Unnamed => self.relist(vfs, under, effects),
                _ => self.rescan(vfs, under, effects),
            }
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
        let disk = self.disk(vfs);
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
    /// stamped, and the directories whose stamp moved are listed. The column's names are swept
    /// as they are ([`Batch::names`]); of the head, only what changed since the column held all
    /// of it is added (all of it when the column never did).
    pub fn rescan(&mut self, vfs: &Vfs, under: FileName, effects: &mut Vec<Effect>) {
        let Some(root) = self.rel(&under).map(str::to_owned) else { return };
        let head = self.head_root(vfs);
        let disk = self.disk(vfs);
        let mut batch = Batch::new(Vec::new(), Some(root.clone()), None);
        if let Some(disk) = disk {
            disk.sweep(under.as_str(), &mut batch.names, &mut batch.saved);
        }
        let inside = |path: &str| {
            root.is_empty()
                || path.strip_prefix(root.as_str()).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        };
        let mut add = |path: &str, batch: &mut Batch| {
            let name = if path.is_empty() { self.dir.to_string() } else { format!("{}{path}", self.below) };
            match batch.swept(&name) {
                Ok(at) => batch.forced.push(at as u32),
                Err(_) if batch.index.contains_key(name.as_str()) => {}
                Err(_) => {
                    let file = FileName::new(&name).expect("a head's path is a name");
                    let was = seen(&vfs.tree, head, path);
                    batch.index.insert(name.into(), batch.slots.len());
                    batch.slots.push(Slot { path: path.to_owned(), file, was, saved: None, stamp: None, look: None });
                }
            }
        };
        if batch.swept(if root.is_empty() { self.dir.as_str() } else { under.as_str() }).is_err() {
            add(&root, &mut batch);
        }
        match disk.and_then(|it| it.root).and_then(|it| vfs.tree.node(&it)) {
            Some(synced) => {
                for change in vfs.tree.diff(synced, head) {
                    if inside(&change.path) {
                        add(&change.path, &mut batch);
                    }
                }
            }
            None => {
                if let Some(node) = vfs.tree.lookup(head, &root) {
                    for (path, ..) in vfs.tree.files(node, Mode::Regular, &root) {
                        add(&path, &mut batch);
                    }
                }
            }
        }
        batch.forced.sort_unstable();
        batch.forced.dedup();
        let mut names = batch.names.clone();
        for slot in &batch.slots {
            names.push(&slot.file);
        }
        let number = self.push(batch);
        let tag = self.tag(Pending::Stamp(number));
        effects.push(Effect::Stamp { files: names, tag });
    }

    /// Lists the directory `dir` alone again, for [`Lost::Unnamed`]: a name directly in it that
    /// the column or the head has and the listing has not is gone with what is under it, a
    /// name new to it is taken whole, and the rest are as they were.
    fn relist(&mut self, vfs: &Vfs, dir: FileName, effects: &mut Vec<Effect>) {
        let Some(root) = self.rel(&dir).map(str::to_owned) else { return };
        let head = self.head_root(vfs);
        let below = self.below.len();
        let mut slots: Vec<Slot> = Vec::new();
        let mut index: FxHashMap<Box<str>, usize> = FxHashMap::default();
        if let Some(disk) = self.disk(vfs) {
            disk.children(dir.as_str(), |name, saved| {
                let file = FileName::new(name).expect("a column's name is a name");
                let path = name[below..].to_owned();
                let was = seen(&vfs.tree, head, &path);
                index.insert(name.into(), slots.len());
                slots.push(Slot { path, file, was, saved: Some(saved), stamp: None, look: None });
            });
        }
        if let Some(node) = vfs.tree.lookup(head, &root)
            && let Some(entries) = vfs.tree.entries(node)
        {
            for (name, child, mode) in entries.iter() {
                let name = vfs.tree.name_str(name);
                let path = if root.is_empty() { name.to_owned() } else { format!("{root}/{name}") };
                let full = format!("{}{path}", self.below);
                if index.contains_key(full.as_str()) {
                    continue;
                }
                let file = FileName::new(&full).expect("a head's path is a name");
                index.insert(full.into(), slots.len());
                let was = Some((vfs.tree.id(child), mode));
                slots.push(Slot { path, file, was, saved: None, stamp: None, look: None });
            }
        }
        let mut batch = Batch::new(slots, None, Some(dir.clone()));
        batch.index = index;
        let number = self.push(batch);
        let tag = self.tag(Pending::Entries(number, dir.clone()));
        effects.push(Effect::Entries { dir, tag });
    }

    /// A batch of `slots`, and the stamp of each asked for in one effect. Returns its number.
    fn ask_stamps(&mut self, slots: Vec<Slot>, rescan: Option<String>, effects: &mut Vec<Effect>) -> u64 {
        let mut names = Names::with_capacity(slots.len(), slots.len() * 32);
        for slot in &slots {
            names.push(&slot.file);
        }
        let number = self.push(Batch::new(slots, rescan, None));
        let tag = self.tag(Pending::Stamp(number));
        effects.push(Effect::Stamp { files: names, tag });
        number
    }

    /// The stamps of a batch: what is unchanged is settled, a file new to its path (or renamed
    /// over it) with a gone file's stamp is that file moved, what else moved is read, and each
    /// directory that is new, replaced, or (in a rescan) moved is listed.
    fn stamped(
        &mut self,
        vfs: &Vfs,
        number: u64,
        stamps: Vec<Stamp>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Watched>,
    ) {
        let head = self.head_root(vfs);
        let disk = self.disk(vfs);
        let below = self.below.len();
        let mut reads = Vec::new();
        let mut lists = Vec::new();
        let mut stop = false;
        {
            let Some(batch) = self.batch(number) else { unreachable!("stamps of no batch") };
            let swept = batch.names.len();
            assert_eq!(stamps.len(), swept + batch.slots.len(), "a stamp per name");
            let rescan = batch.rescan.is_some();
            batch.open -= 1;
            let (of_swept, of_slots) = stamps.split_at(swept);
            for (slot, stamp) in batch.slots.iter_mut().zip(of_slots) {
                slot.stamp = Some(*stamp);
            }
            let mut forced = batch.forced.iter().copied().peekable();
            for (at, (saved, now)) in batch.saved.iter().zip(of_swept).enumerate() {
                let force = forced.next_if_eq(&(at as u32)).is_some();
                if saved == now && !force {
                    continue;
                }
                let name = batch.names.get(at);
                let file = FileName::new(name).expect("a column's name is a name");
                let path = name.get(below..).unwrap_or_default().to_owned();
                let was = seen(&vfs.tree, head, &path);
                let slot = Slot { path, file, was, saved: Some(*saved), stamp: Some(*now), look: None };
                batch.slots.push(slot);
            }
            let mut changed = Vec::new();
            for (at, slot) in batch.slots.iter_mut().enumerate() {
                let stamp = slot.stamp.expect("stamped above");
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
                    changed.push(at);
                }
            }
            if !lists.is_empty() {
                batch.indexed();
            }
            batch.went(disk, &vfs.tree, head, below);
            for at in changed {
                let slot = &batch.slots[at];
                let stamp = slot.stamp.expect("stamped above");
                // A new name, or one another file was renamed over (it has another inode).
                let from = match slot.saved {
                    Some(saved) if saved.ino == stamp.ino => None,
                    _ => batch.moved(&stamp),
                };
                match from {
                    Some(from) => batch.slots[at].look = Some(Look::Moved(from)),
                    None => reads.push((at, slot.file.clone())),
                }
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
    /// moved there if it has a gone file's stamp, else read if a file and listed if a
    /// directory. The listing of a relisted directory first settles what was in it.
    fn listed(
        &mut self,
        vfs: &Vfs,
        number: u64,
        dir: &FileName,
        entries: Vec<(FileName, Stamp)>,
        effects: &mut Vec<Effect>,
    ) {
        let head = self.head_root(vfs);
        let disk = self.disk(vfs);
        let below = self.below.len();
        let mut asks = Vec::new();
        let mut rescans = Vec::new();
        {
            let Some(batch) = self.batch(number) else { unreachable!("a listing of no batch") };
            batch.open -= 1;
            if batch.relist.as_ref() == Some(dir) {
                let there: FxHashMap<&str, Stamp> =
                    entries.iter().map(|(it, stamp)| (it.as_str(), *stamp)).collect();
                for (at, slot) in batch.slots.iter_mut().enumerate().filter(|(_, it)| it.look.is_none()) {
                    let Some(&now) = there.get(slot.file.as_str()) else {
                        slot.stamp = Some(Stamp::GONE);
                        slot.look = Some(Look::Gone);
                        continue;
                    };
                    if now.is_dir() {
                        let kept = slot.saved.is_some_and(|it| it.is_dir() && it.ino == now.ino);
                        if !kept {
                            rescans.push(slot.file.clone());
                        }
                        slot.look = Some(Look::Same);
                    } else if slot.saved == Some(now) && slot.was.is_some() {
                        slot.look = Some(Look::Same);
                    } else {
                        slot.stamp = Some(now);
                        asks.push((Some(at), slot.file.clone()));
                    }
                }
                batch.went(disk, &vfs.tree, head, below);
            }
            for (file, stamp) in entries {
                if batch.known(file.as_str()) || file.as_str().len() <= below {
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
                } else if let Some(from) = batch.moved(&stamp) {
                    Some(Look::Moved(from))
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
        for under in rescans {
            self.rescan(vfs, under, effects);
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
            let scanned = batch.rescan.is_some() || batch.relist.is_some() || batch.caught_up;
            let again = self.commit(vfs, batch, out);
            if scanned {
                out.push(Watched::Scanned);
            }
            for file in again {
                vfs.disks.of(&self.head, &self.dir).set(file.as_str(), Stamp::GONE);
                let path = self.rel(&file).expect("a slot's name is under the directory").to_owned();
                out.push(Watched::Lost { path, why: Lost::Rescan });
                self.rescan(vfs, file, effects);
            }
        }
        if self.batches.is_empty() {
            self.ours.clear();
        }
        vfs.flush(effects);
    }

    /// Commits what a batch saw; returns the files moved from a node the tree no longer has,
    /// to be looked at again.
    fn commit(&mut self, vfs: &mut Vfs, mut batch: Batch, out: &mut Vec<Watched>) -> Vec<FileName> {
        let disk = vfs.disks.of(&self.head, &self.dir);
        save(disk, &batch);
        let base = vfs.heads.get(&*self.head).copied().unwrap_or(vfs.tree.empty());
        let tree = &vfs.tree;
        let mut deletes: Vec<String> = Vec::new();
        let mut writes: Vec<(String, Vec<u8>, Mode)> = Vec::new();
        let mut moves: Vec<(usize, usize)> = Vec::new();
        let mut taken = Vec::new();
        // The gone slots this commit deletes: a file moved from one of them is renamed.
        let mut deleting = vec![false; batch.slots.len()];
        for (at, slot) in batch.slots.iter_mut().enumerate() {
            if slot.path.is_empty() || matches!(slot.look, Some(Look::Same | Look::Unreadable)) {
                continue;
            }
            let entry = tree.entry(base, &slot.path);
            let node = entry.map(|it| it.0);
            let now = entry.map(|(node, mode)| (tree.id(node), mode));
            if now != slot.was && !self.made(&slot.path, now) {
                out.push(Watched::Conflict { path: slot.path.clone() });
                continue;
            }
            let is_file = node.is_some_and(|it| !tree.is_dir(it));
            match slot.look.take().expect("a looked-at path") {
                Look::Same | Look::Unreadable => unreachable!("skipped above"),
                Look::Gone if node.is_some() => {
                    deletes.push(slot.path.clone());
                    deleting[at] = true;
                }
                Look::Gone => {}
                Look::Dir if is_file => deletes.push(slot.path.clone()),
                Look::Dir => {}
                Look::Moved(from) => {
                    moves.push((at, from));
                    continue;
                }
                Look::File(bytes) => {
                    room(tree, base, &slot.path, &mut deletes);
                    let mode = entry.filter(|_| is_file).map_or(Mode::Regular, |it| it.1);
                    // Compared in place, not hashed: the commit hashes what it writes, once. A
                    // blob of the image that fails its check compares unequal, so it is replaced.
                    let same = node
                        .filter(|_| is_file)
                        .and_then(|it| tree.bytes(it).ok().flatten())
                        .is_some_and(|it| *it == *bytes);
                    if !same {
                        writes.push((slot.path.clone(), bytes, mode));
                    }
                }
            }
            taken.push(slot.path.clone());
        }

        let mut again = Vec::new();
        let mut renames: Vec<(String, String)> = Vec::new();
        let mut renamed = FxHashSet::default();
        for (at, from) in moves {
            let (target, source) = (&batch.slots[at], &batch.slots[from]);
            let Some((id, mode)) = source.was else { unreachable!("a move is from a file the head had") };
            let entry = tree.entry(base, &target.path);
            if entry.is_some_and(|(node, _)| tree.id(node) == id) {
                taken.push(target.path.clone());
                continue;
            }
            let clear = entry.is_none()
                && !ancestors(&target.path).any(|up| tree.lookup(base, up).is_some_and(|it| !tree.is_dir(it)));
            if deleting[from] && clear && renamed.insert(from) {
                renames.push((source.path.clone(), target.path.clone()));
            } else {
                match tree.node(&id).map(|node| tree.bytes(node)) {
                    Some(Ok(Some(bytes))) => {
                        room(tree, base, &target.path, &mut deletes);
                        writes.push((target.path.clone(), bytes.to_vec(), mode));
                    }
                    _ => {
                        again.push(target.file.clone());
                        continue;
                    }
                }
            }
            taken.push(target.path.clone());
        }

        deletes.sort_unstable();
        deletes.dedup();
        // A path under one deleted goes with it.
        let deleted = |path: &str| deletes.binary_search_by(|it| it.as_str().cmp(path)).is_ok();
        let deletes: Vec<String> =
            deletes.iter().filter(|path| !ancestors(path).any(deleted)).cloned().collect();
        // A rename moves its source out first: a delete at or above one waits for the renames,
        // and is left out when nothing else is under it.
        let mut sources: Vec<&str> = renames.iter().map(|(from, _)| from.as_str()).collect();
        sources.sort_unstable();
        let source = |path: &str| sources.binary_search(&path).is_ok();
        let covers = |path: &str| {
            let under = format!("{path}/");
            source(path)
                || sources
                    .get(sources.partition_point(|it| *it < under.as_str()))
                    .is_some_and(|it| it.starts_with(&under))
        };
        let (mut early, mut late) = (Vec::new(), Vec::new());
        for path in deletes {
            if !covers(&path) {
                early.push(path);
            } else if !source(&path)
                && tree.lookup(base, &path).is_some_and(|dir| {
                    tree.files(dir, Mode::Regular, &path).iter().any(|(file, ..)| !source(file))
                })
            {
                late.push(path);
            }
        }
        let ops: Vec<Op> = early
            .into_iter()
            .map(|path| Op::Delete { path })
            .chain(renames.into_iter().map(|(from, to)| Op::Rename { to, from }))
            .chain(late.into_iter().map(|path| Op::Delete { path }))
            .chain(writes.into_iter().map(|(path, bytes, mode)| Op::Write { path, bytes, mode }))
            .collect();
        if ops.is_empty() {
            return again;
        }
        let base = vfs.tree.id(base);
        let commit = vfs.commit(&self.head, base, &ops).unwrap_or_else(|error| {
            panic!("the disk's changes to {} do not apply to its head: {error}", self.dir)
        });
        let root = vfs.root(commit.root).expect("the root just made");
        for path in taken.iter().map(String::as_str).chain(ops.iter().flat_map(op_paths)) {
            self.ours.insert(path.to_owned(), seen(&vfs.tree, root, path));
        }
        out.push(Watched::Committed(commit));
        again
    }

    /// Whether this watcher's commits left `path` as `now`: it put that there, or `path` is gone
    /// because it replaced or deleted something above it.
    fn made(&self, path: &str, now: Seen) -> bool {
        self.ours.get(path) == Some(&now)
            || (now.is_none() && ancestors(path).any(|up| self.ours.contains_key(up)))
    }
}

/// Makes room in `base` for a file at `path`: the files above it and a directory at it go.
fn room(tree: &Tree, base: NodeId, path: &str, deletes: &mut Vec<String>) {
    for up in ancestors(path) {
        if tree.lookup(base, up).is_some_and(|it| !tree.is_dir(it)) {
            deletes.push(up.to_owned());
        }
    }
    if tree.lookup(base, path).is_some_and(|it| tree.is_dir(it)) {
        deletes.push(path.to_owned());
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
            Some(Look::Dir | Look::File(_) | Look::Moved(_)) => disk.set(slot.file.as_str(), stamp),
        }
    }
    if let Some(cursor) = batch.cursor {
        disk.cursor = cursor;
    }
}

#[cfg(test)]
mod tests;
