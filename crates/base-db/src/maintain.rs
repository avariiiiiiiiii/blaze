//! Write-side view maintenance (docs/v2/plan/design/dbsp.md §3): every write to a [`Db`] input
//! ends by running the [`Maintainer`]s over what it [`Written`], and each maintainer updates
//! its state by that delta and sets the query inputs whose slice changed. Queries never read a
//! maintainer's state, only the inputs it sets, so every query stays a pure function of inputs.
//!
//! A write method of [`Db`] notes what it changed into the database's pending [`Written`];
//! outside a [`Db::batch`] it then runs the maintainers, inside one the batch's end does, once,
//! over everything the batch wrote. Maintainers run in `order`, each once per pass. Writes a
//! maintainer makes itself are not noted and run no maintenance: what a maintainer sets is its
//! own output, not a change to the program.
//!
//! A panic in a write or a maintainer drops every maintainer's state: the next write maintains
//! from empty, which is the cold run, not another path.

use std::{
    any::{Any, TypeId},
    panic::{self, AssertUnwindSafe},
    sync::OnceLock,
};

use blaze_dbsp::Work;
use rustc_hash::FxHashMap;
use span::FileId;

use crate::{Crate, Db, IncludedFile, SourceRoot, SourceRootId};

#[cfg(test)]
mod tests;

/// What one write changed: what a maintainer reads to find its delta. Each list is sorted and
/// has each item once; a file in `added` or `removed` is not also in `set`.
#[derive(Clone, Copy, Debug)]
pub struct Written<'a> {
    /// Files whose text changed.
    pub set: &'a [FileId],
    pub added: &'a [FileId],
    pub removed: &'a [FileId],
    /// Source roots whose file sets changed (a `mod` probe may now hit).
    pub roots: &'a [SourceRootId],
    /// The crate graph, cfg, env or proc macros changed.
    pub crates: bool,
    /// Files `include_bytes!` or `include_str!` read whose bytes changed.
    pub included: &'a [IncludedFile],
}

impl Written<'_> {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
            && self.added.is_empty()
            && self.removed.is_empty()
            && self.roots.is_empty()
            && !self.crates
            && self.included.is_empty()
    }
}

/// A write-side view maintainer, registered with `inventory::submit!` by the crate with the code
/// (as `hir-ty/src/providers.rs` registers MIR): nothing installs anything.
pub struct Maintainer {
    /// When it runs among the others: lower first. hir-def's name resolution is `100`, hir-ty's
    /// impls and trait candidates `200`.
    pub order: u16,
    pub name: &'static str,
    pub run: fn(&mut Db, &Written<'_>),
}

inventory::collect!(Maintainer);

/// What a maintainer did for one crate in one maintenance pass ([`Db::report_work`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Maintained {
    /// Which pass since the records were last taken, from 0: one write outside a batch, or one
    /// batch.
    pub write: u32,
    /// [`Maintainer::name`].
    pub maintainer: &'static str,
    pub krate: Crate,
    pub work: Work,
}

/// The maintainers linked into this binary, in `order` (then name), sorted once.
pub fn maintainers() -> &'static [&'static Maintainer] {
    static SORTED: OnceLock<Box<[&'static Maintainer]>> = OnceLock::new();
    SORTED.get_or_init(|| {
        // alloc: once per process, the sorted list of linked maintainers
        let mut all: Vec<&'static Maintainer> = inventory::iter::<Maintainer>().collect();
        all.sort_by_key(|maintainer| (maintainer.order, maintainer.name));
        for pair in all.windows(2) {
            assert!(
                pair[0].name != pair[1].name,
                "two maintainers are named `{}`",
                pair[0].name
            );
        }
        all.into_boxed_slice()
    })
}

/// The [`Db`] field behind maintenance.
#[derive(Default)]
pub(crate) struct Maintenance {
    // dyn: write-side state, one lookup per maintainer per write
    state: FxHashMap<TypeId, Box<dyn Any + Send + Sync>>,
    pending: Pending,
    /// How many [`Db::batch`]es are open.
    depth: u32,
    /// Whether the maintainers are running: their own writes are not noted.
    maintaining: bool,
    /// Whether [`Db::report_work`] keeps its records ([`Db::keep_maintained`]).
    keep: bool,
    /// The passes run since the records were last taken.
    passes: u32,
    maintained: Vec<Maintained>,
}

/// A pending [`Written`], owned. Its buffers are kept from one pass to the next.
#[derive(Default)]
struct Pending {
    set: Vec<FileId>,
    added: Vec<FileId>,
    removed: Vec<FileId>,
    roots: Vec<SourceRootId>,
    crates: bool,
    included: Vec<IncludedFile>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.written().is_empty()
    }

    fn written(&self) -> Written<'_> {
        Written {
            set: &self.set,
            added: &self.added,
            removed: &self.removed,
            roots: &self.roots,
            crates: self.crates,
            included: &self.included,
        }
    }

    /// Sorts and dedups each list, and takes the files added or removed out of `set`.
    fn normalize(&mut self) {
        for files in [&mut self.set, &mut self.added, &mut self.removed] {
            files.sort_unstable();
            files.dedup();
        }
        self.roots.sort_unstable();
        self.roots.dedup();
        let (added, removed) = (&self.added, &self.removed);
        self.set.retain(|file| {
            added.binary_search(file).is_err() && removed.binary_search(file).is_err()
        });
    }

    fn clear(&mut self) {
        self.set.clear();
        self.added.clear();
        self.removed.clear();
        self.roots.clear();
        self.crates = false;
        self.included.clear();
    }
}

/// What a write method notes it changed ([`Db::note`]).
pub(crate) enum Note<'a> {
    Set(FileId),
    View { set: &'a [FileId], added: &'a [FileId], removed: &'a [FileId], roots: &'a [SourceRootId] },
    Root(SourceRootId),
    Crates,
    Included(IncludedFile),
}

impl Db {
    /// Runs `f` with this database and its write-side state `T` (made by `Default`); the state is
    /// taken out for the call, so `f` may read queries with `&*db` and set inputs with `db`.
    /// No query can reach it. If `f` unwinds, the state is dropped, and the next call starts
    /// from `T::default()`.
    ///
    /// # Panics
    ///
    /// When `f` asks for the same `T` again: it is out for this call.
    pub fn with_state<T: Default + Send + Sync + 'static, R>(
        &mut self,
        f: impl FnOnce(&mut Db, &mut T) -> R,
    ) -> R {
        let id = TypeId::of::<T>();
        let mut state = match self.maintenance.state.get_mut(&id) {
            Some(slot) => {
                // One placeholder per state while it is out: a zero-sized `Box` allocates nothing.
                let taken = std::mem::replace(slot, Box::new(InUse)); // alloc: zero-sized, allocates nothing
                match taken.downcast::<T>() {
                    Ok(state) => state,
                    Err(_) => panic!(
                        "`Db::with_state::<{}>` inside a call that has it out",
                        std::any::type_name::<T>()
                    ),
                }
            }
            None => {
                // alloc: a maintainer's state, once per database (and after a panic dropped it)
                let state = Box::new(T::default());
                self.maintenance.state.insert(id, Box::new(InUse)); // alloc: zero-sized, allocates nothing
                state
            }
        };
        match panic::catch_unwind(AssertUnwindSafe(|| f(self, &mut *state))) {
            Ok(result) => {
                let state: Box<dyn Any + Send + Sync> = state; // dyn: write-side state, one lookup per maintainer per write
                self.maintenance.state.insert(id, state);
                result
            }
            Err(panic) => {
                self.maintenance.state.remove(&id);
                drop(state);
                panic::resume_unwind(panic)
            }
        }
    }

    /// Runs `f` with maintenance held back, then runs each maintainer once over everything `f`
    /// wrote. Every write method outside a batch is its own batch; a batch inside another, or
    /// inside a maintainer, is part of the one around it.
    pub fn batch<R>(&mut self, f: impl FnOnce(&mut Db) -> R) -> R {
        self.maintenance.depth += 1;
        let ran = panic::catch_unwind(AssertUnwindSafe(|| f(self)));
        self.maintenance.depth -= 1;
        match ran {
            Ok(result) => {
                if self.maintenance.depth == 0 && !self.maintenance.maintaining {
                    self.maintain();
                }
                result
            }
            Err(panic) => {
                // The inputs may be half written: what is maintained no longer follows them.
                self.forget_maintained_state();
                panic::resume_unwind(panic)
            }
        }
    }

    /// Notes that an input of the crate graph changed that base-db does not set itself (a
    /// crate's proc macros), as a write: outside a [`Db::batch`] the maintainers run now.
    pub fn note_crates_changed(&mut self) {
        self.batch(|db| db.note(Note::Crates));
    }

    /// Records what `maintainer` did for `krate` in this pass, while [`Db::keep_maintained`] is
    /// on; a maintainer reports each crate it went over once per pass, zeros included.
    pub fn report_work(&mut self, maintainer: &'static str, krate: Crate, work: Work) {
        let maintenance = &mut self.maintenance;
        if maintenance.keep {
            let write = maintenance.passes.saturating_sub(1);
            // alloc: a record per crate per pass, only while a request keeps them
            maintenance.maintained.push(Maintained { write, maintainer, krate, work });
        }
    }

    /// Whether [`Db::report_work`] keeps its records from now on (off at first): the daemon
    /// turns it on for a request that prints them, a cost test for what it asserts.
    pub fn keep_maintained(&mut self, keep: bool) {
        self.maintenance.keep = keep;
        if !keep {
            self.maintenance.maintained.clear();
        }
    }

    /// Appends the records kept since the last call to `out` and forgets them; the next pass is
    /// `write` 0 again.
    pub fn take_maintained(&mut self, out: &mut Vec<Maintained>) {
        out.append(&mut self.maintenance.maintained);
        self.maintenance.passes = 0;
    }

    /// Notes what a write method changed; nothing while the maintainers run.
    pub(crate) fn note(&mut self, note: Note<'_>) {
        let maintenance = &mut self.maintenance;
        if maintenance.maintaining {
            return;
        }
        let pending = &mut maintenance.pending;
        match note {
            Note::Set(file) => pending.set.push(file),
            Note::View { set, added, removed, roots } => {
                pending.set.extend_from_slice(set);
                pending.added.extend_from_slice(added);
                pending.removed.extend_from_slice(removed);
                pending.roots.extend_from_slice(roots);
            }
            Note::Root(root) => pending.roots.push(root),
            Note::Crates => pending.crates = true,
            Note::Included(file) => {
                if !pending.included.contains(&file) {
                    pending.included.push(file);
                }
            }
        }
    }

    /// Notes a source root set from `old` (`None` for a new root) to `new`: the root, and the
    /// files that came into it or left it. `O(files of both)`, on a root's set, which is setup
    /// or a view change.
    pub(crate) fn note_root(&mut self, id: SourceRootId, old: Option<&SourceRoot>, new: &SourceRoot) {
        let maintenance = &mut self.maintenance;
        if maintenance.maintaining {
            return;
        }
        let pending = &mut maintenance.pending;
        pending.roots.push(id);
        match old {
            Some(old) => {
                pending.added.extend(new.iter().filter(|file| old.path_for_file(file).is_none()));
                pending.removed.extend(old.iter().filter(|file| new.path_for_file(file).is_none()));
            }
            None => pending.added.extend(new.iter()),
        }
    }

    /// Runs `f` as a write of crate inputs: the crate graph's content ID (of every crate's
    /// data, cfg, env and dependencies) before and after tells whether it changed anything.
    pub(crate) fn write_crates<R>(&mut self, f: impl FnOnce(&mut Db) -> R) -> R {
        self.batch(|db| {
            let before = crate::all_crates_content(&*db);
            let result = f(db);
            if crate::all_crates_content(&*db) != before {
                db.note(Note::Crates);
            }
            result
        })
    }

    /// Runs `f` as a write of `file`'s text input, noted when its content ID changed.
    pub(crate) fn write_file<R>(&mut self, file: FileId, f: impl FnOnce(&mut Db) -> R) -> R {
        self.batch(|db| {
            let before = db.files().entry_content(&*db, file);
            let result = f(db);
            if db.files().entry_content(&*db, file) != before {
                db.note(Note::Set(file));
            }
            result
        })
    }

    /// Runs every maintainer once over the pending writes, then clears them.
    fn maintain(&mut self) {
        if self.maintenance.pending.is_empty() {
            return;
        }
        let all = maintainers();
        if all.is_empty() {
            self.maintenance.pending.clear();
            return;
        }
        self.maintenance.pending.normalize();
        self.maintenance.passes = self.maintenance.passes.saturating_add(1);
        let pending = std::mem::take(&mut self.maintenance.pending);
        self.maintenance.maintaining = true;
        let ran = panic::catch_unwind(AssertUnwindSafe(|| {
            let written = pending.written();
            for maintainer in all {
                (maintainer.run)(self, &written);
            }
        }));
        self.maintenance.maintaining = false;
        let mut pending = pending;
        pending.clear();
        self.maintenance.pending = pending;
        if let Err(panic) = ran {
            self.forget_maintained_state();
            panic::resume_unwind(panic);
        }
    }

    /// Drops every maintainer's state, so the next write maintains from empty, and has it
    /// re-check everything.
    fn forget_maintained_state(&mut self) {
        self.maintenance.state.clear();
        self.maintenance.pending.crates = true;
    }
}

/// What a state's slot holds while [`Db::with_state`] has it out.
struct InUse;
