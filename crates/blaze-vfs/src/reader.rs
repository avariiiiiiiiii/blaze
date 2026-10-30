//! [`Reader`]: the tree's nodes by content ID, read from any thread while the [`Vfs`] goes on,
//! by the rules of docs/v2/plan/design/concurrency.md: a read is acquire loads and writes no
//! shared memory, the table is append-only, and memory is reclaimed at task boundaries.
//!
//! - The nodes are in an open-addressed table keyed by the content ID's bits. A place goes
//!   from empty to a pointer to an immutable node once, by one release store, and is never
//!   changed after. A full table is not rehashed in place: a new level twice its size is
//!   filled and swapped in by one release store of the table's pointer, and the old level is
//!   retired.
//! - There is one writer, the [`Vfs`] (through its [`Publisher`], which takes `&mut`): it
//!   publishes the nodes each change made, children before parents, so a level that has a
//!   directory has everything under it. Collection is a new level of just the nodes the tree
//!   kept, swapped in, with the old level and the dropped nodes retired.
//! - What is retired is freed only by [`Vfs::reclaim`], once no read begun before is still
//!   going (quiescent-state reclamation: the daemon calls it once every worker has passed a
//!   task boundary), or when the last handle goes.
//! - A read borrows: a file's bytes are the tree's own [`Arc`], lent without touching its
//!   count. A root that a head, an open view or a collection's `keep` holds is never retired,
//!   so what is read of it stays valid while it is held.
//!
//! [`Vfs`]: crate::Vfs
//! [`Vfs::reclaim`]: crate::Vfs::reclaim

use std::{
    ptr,
    sync::{
        Arc,
        atomic::{AtomicPtr, Ordering},
    },
};

use blaze_ids::ContentId;

use crate::Mode;

/// A directory's entry as the reader holds it.
pub(crate) struct Entry {
    pub(crate) name: Arc<str>,
    pub(crate) id: ContentId,
    /// [`Mode::Regular`] for a directory.
    pub(crate) mode: Mode,
    pub(crate) dir: bool,
}

pub(crate) enum Kind {
    File(Arc<[u8]>),
    /// Sorted by name, as the tree's directory is.
    Dir(Box<[Entry]>),
}

pub(crate) struct Node {
    pub(crate) id: ContentId,
    pub(crate) kind: Kind,
}

/// The smallest level, and the most a level is filled: half its places.
const FIRST: usize = 16;

struct Level {
    /// Empty, or a node published once and never changed.
    places: Box<[AtomicPtr<Node>]>,
}

impl Level {
    fn new(len: usize) -> Level {
        Level { places: (0..len).map(|_| AtomicPtr::new(ptr::null_mut())).collect() }
    }

    /// The node of `id`, or the empty place its probe ended at. A level is at most half full,
    /// so a probe ends.
    fn probe(&self, id: &ContentId, order: Ordering) -> Result<*mut Node, usize> {
        let mask = self.places.len() - 1;
        let mut word = [0; 8];
        word.copy_from_slice(&id.0[8..16]);
        let mut at = u64::from_le_bytes(word) as usize & mask;
        loop {
            let node = self.places[at].load(order);
            if node.is_null() {
                return Err(at);
            }
            // SAFETY: a published node is freed only after its level is retired and every
            // read begun before has ended (`Vfs::reclaim`'s contract), and this read holds
            // the level.
            if unsafe { &(*node).id } == id {
                return Ok(node);
            }
            at = (at + 1) & mask;
        }
    }
}

/// What the writer took out of the table, freed by [`Publisher::reclaim`] or the table's drop.
enum Gone {
    Level(*mut Level),
    Node(*mut Node),
}

// SAFETY: a `Gone` owns what it points to, which is `Send` and `Sync` (`Node` and `Level`
// hold only `Arc`s, boxes, atomics and plain data); no other thread frees or writes it, and a
// shared `&Gone` reaches nothing.
unsafe impl Send for Gone {}
unsafe impl Sync for Gone {}

impl Drop for Gone {
    fn drop(&mut self) {
        // SAFETY: each was made by `Box::into_raw`, retired once, and is freed once, here,
        // when no read can be using it (the reclaim's contract, or the table's last handle).
        match *self {
            Gone::Level(level) => drop(unsafe { Box::from_raw(level) }),
            Gone::Node(node) => drop(unsafe { Box::from_raw(node) }),
        }
    }
}

struct Table {
    /// The level in use; swapped only by the table's one [`Publisher`].
    level: AtomicPtr<Level>,
    /// What the publisher had retired and not reclaimed when it was dropped.
    orphans: AtomicPtr<Vec<Gone>>,
}

const fn shared<T: Send + Sync>() {}
const _: () = shared::<Node>();
const _: () = shared::<Level>();

impl Drop for Table {
    fn drop(&mut self) {
        // SAFETY: the last handle is gone, so nothing reads. The nodes in the level in use
        // are owned by it alone: a node is retired only when a level without it is swapped in.
        let level = unsafe { Box::from_raw(*self.level.get_mut()) };
        for place in &level.places {
            let node = place.load(Ordering::Relaxed);
            if !node.is_null() {
                drop(unsafe { Box::from_raw(node) });
            }
        }
        let orphans = *self.orphans.get_mut();
        if !orphans.is_null() {
            // SAFETY: made by `Box::into_raw` in the publisher's drop, taken once.
            drop(unsafe { Box::from_raw(orphans) });
        }
    }
}

/// A handle on every node the [`Vfs`](crate::Vfs) has, by content ID: cheap to clone, `Send`
/// and `Sync`. A read takes no lock and writes no shared memory. A root is readable while the
/// tree has it (a head reaches it, a view of it is open, or a collection kept it); after that
/// it reads as none.
#[derive(Clone)]
pub struct Reader {
    table: Arc<Table>,
}

impl Reader {
    fn level(&self) -> &Level {
        // SAFETY: a level is freed only once retired and every read begun before has ended
        // (`Vfs::reclaim`'s contract), or with the table, which `&self` keeps.
        unsafe { &*self.table.level.load(Ordering::Acquire) }
    }

    /// The node at `path` under the directory `root`, in one level from start to end, and the
    /// mode of the entry naming it.
    fn walk<'a>(level: &'a Level, root: ContentId, path: &str) -> Option<(&'a Node, Mode)> {
        // SAFETY: as in `Level::probe`; the borrow is of the reader's `&self`, and the
        // reclaim's contract keeps the node while it lives.
        let node = |id: &ContentId| level.probe(id, Ordering::Acquire).ok().map(|it| unsafe { &*it });
        let start = node(&root).filter(|it| matches!(it.kind, Kind::Dir(_)))?;
        if path.is_empty() {
            return Some((start, Mode::Regular));
        }
        path.split('/').try_fold((start, Mode::Regular), |(at, _), name| {
            let Kind::Dir(entries) = &at.kind else { return None };
            let entry = &entries[entries.binary_search_by(|it| (*it.name).cmp(name)).ok()?];
            Some((node(&entry.id)?, entry.mode))
        })
    }

    /// The file at `path` under `root`: its content ID, mode and bytes (a symlink's are its
    /// target, not followed); `None` for none, a directory, or a root the tree does not have.
    /// The bytes are lent: clone the [`Arc`] to keep them past the root.
    pub fn file(&self, root: ContentId, path: &str) -> Option<(ContentId, Mode, &Arc<[u8]>)> {
        match Self::walk(self.level(), root, path)? {
            (Node { id, kind: Kind::File(bytes) }, mode) => Some((*id, mode, bytes)),
            _ => None,
        }
    }

    /// The names in the directory at `path` (`""` for the root), sorted, each with whether it
    /// is a directory; `None` for none, a file, or a root the tree does not have.
    pub fn list(&self, root: ContentId, path: &str) -> Option<impl Iterator<Item = (&str, bool)>> {
        match Self::walk(self.level(), root, path)? {
            (Node { kind: Kind::Dir(entries), .. }, _) => {
                Some(entries.iter().map(|it| (&*it.name, it.dir)))
            }
            _ => None,
        }
    }
}

/// The one writer of a [`Reader`]'s table, owned by the [`Vfs`](crate::Vfs).
pub(crate) struct Publisher {
    table: Arc<Table>,
    /// Nodes in the level in use.
    len: usize,
    retired: Vec<Gone>,
}

impl Publisher {
    pub(crate) fn new() -> Publisher {
        let level = Box::into_raw(Box::new(Level::new(FIRST)));
        let table =
            Arc::new(Table { level: AtomicPtr::new(level), orphans: AtomicPtr::new(ptr::null_mut()) });
        Publisher { table, len: 0, retired: Vec::new() }
    }

    pub(crate) fn reader(&self) -> Reader {
        Reader { table: self.table.clone() }
    }

    fn level(&self) -> &Level {
        // SAFETY: only this publisher swaps or retires a level, and it frees one only in
        // `reclaim`, which takes `&mut self`.
        unsafe { &*self.table.level.load(Ordering::Relaxed) }
    }

    /// Publishes `made`, children before parents; a node the table has already is skipped.
    pub(crate) fn publish(&mut self, made: Vec<Node>) {
        for node in made {
            if (self.len + 1) * 2 > self.level().places.len() {
                let len = self.level().places.len() * 2;
                self.rebuild(len, |_| true);
            }
            let level = self.level();
            if let Err(at) = level.probe(&node.id, Ordering::Relaxed) {
                level.places[at].store(Box::into_raw(Box::new(node)), Ordering::Release);
                self.len += 1;
            }
        }
    }

    /// Keeps the nodes `kept` says the tree still has, in a level sized for them; the rest
    /// are retired.
    pub(crate) fn collect(&mut self, kept: impl Fn(&ContentId) -> bool) {
        let live = self.nodes().filter(|&node| kept(&node.id)).count();
        let len = (live * 2 + 1).next_power_of_two().max(FIRST);
        self.rebuild(len, kept);
    }

    fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.level().places.iter().filter_map(|place| {
            // SAFETY: a node in the level in use is freed only by this publisher's `reclaim`.
            unsafe { place.load(Ordering::Relaxed).as_ref() }
        })
    }

    /// Fills a level of `len` places with the nodes `keep` says and swaps it in; the old level
    /// and the nodes left out are retired.
    fn rebuild(&mut self, len: usize, keep: impl Fn(&ContentId) -> bool) {
        let old = self.table.level.load(Ordering::Relaxed);
        let new = Level::new(len);
        let mut count = 0;
        let mut dropped = Vec::new();
        for place in &self.level().places {
            let node = place.load(Ordering::Relaxed);
            if node.is_null() {
                continue;
            }
            // SAFETY: as in `nodes`.
            let id = unsafe { &(*node).id };
            if keep(id) {
                let Err(at) = new.probe(id, Ordering::Relaxed) else {
                    unreachable!("a node published twice")
                };
                new.places[at].store(node, Ordering::Relaxed);
                count += 1;
            } else {
                dropped.push(Gone::Node(node));
            }
        }
        // Readers that load the new level see every store into it before this one.
        self.table.level.store(Box::into_raw(Box::new(new)), Ordering::Release);
        self.retired.push(Gone::Level(old));
        self.retired.extend(dropped);
        self.len = count;
    }

    /// Frees what was retired.
    ///
    /// # Safety
    ///
    /// Every read of a [`Reader`] of this table begun before the retiring (a publish that
    /// grew the table, or a collect) has ended, and nothing it returned of a node the collect
    /// dropped is still used.
    pub(crate) unsafe fn reclaim(&mut self) {
        self.retired.clear();
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        // Readers may outlive the `Vfs`: what they might still be reading is freed with the
        // table.
        if self.retired.is_empty() {
            return;
        }
        let orphans = Box::into_raw(Box::new(std::mem::take(&mut self.retired)));
        let before = self.table.orphans.swap(orphans, Ordering::AcqRel);
        assert!(before.is_null(), "a table has one publisher");
    }
}
