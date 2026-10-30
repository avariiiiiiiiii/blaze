//! What a maintainer run reads: the read set the read index keeps (design §6). Every read of a
//! module goes through `DefMap::module` and `DefMap::name_in_module` of a
//! `ModulesSlot::Maintained` view, or a published scope (`ScopeSlot::Input`, another crate's or
//! one of this crate the run does not hold), and each notes it here. A name read is
//! `(module, Some(name))`, negative or not; anything else (a whole scope, the children, the
//! textual macros, the row) is `(module, None)`.
//!
//! A read is noted with its *reader*: the module whose import the unit is resolving, or whose
//! macro directive it is resolving or collecting the expansion of ([`reading`]). A read made
//! outside both has no reader, and the run counts it as read by every group it holds.
//!
//! The notes go to the calling thread's list, which is on only while [`capture`] runs: a run is
//! on one thread, and the queries it asks that run elsewhere read no maintained view.

use std::cell::{Cell, RefCell};

use hir_expand::name::Name;

use crate::ModuleId;

/// `(reader, (module, name))`.
pub(crate) type Read = (Option<ModuleId>, (ModuleId, Option<Name>));

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static READER: Cell<Option<ModuleId>> = const { Cell::new(None) };
    static READS: RefCell<Vec<Read>> = const { RefCell::new(Vec::new()) }; // alloc: empty, grown once per thread and kept
}

/// Notes a read of `module` (of `name` in it, or of the module whole).
#[inline]
pub(crate) fn note(module: ModuleId, name: Option<Name>) {
    if ON.get() {
        let reader = READER.get();
        // alloc: the thread's list keeps its capacity from run to run.
        READS.with_borrow_mut(|it| it.push((reader, (module, name))));
    }
}

/// Whether a run is capturing on this thread: a published scope notes its reads only then.
#[inline]
pub(crate) fn on() -> bool {
    ON.get()
}

/// The reader reads are noted with until the guard drops, when the one before is back.
#[must_use]
pub(crate) struct Reading {
    before: Option<ModuleId>,
}

impl Drop for Reading {
    fn drop(&mut self) {
        READER.set(self.before);
    }
}

/// Reads from now until the guard drops are `module`'s.
#[inline]
pub(crate) fn reading(module: ModuleId) -> Reading {
    Reading { before: READER.replace(Some(module)) }
}

/// Runs `f`, then moves what it read to the end of `into` (with repeats; the caller sorts).
/// A panic in `f` leaves capturing off and the list empty.
pub(super) fn capture<R>(into: &mut Vec<Read>, f: impl FnOnce() -> R) -> R {
    struct Off;
    impl Drop for Off {
        fn drop(&mut self) {
            ON.set(false);
            READER.set(None);
            READS.with_borrow_mut(|it| it.clear());
        }
    }
    assert!(!ON.get(), "nameres delta: a maintainer run inside another");
    READS.with_borrow_mut(|it| it.clear());
    READER.set(None);
    ON.set(true);
    let off = Off;
    let out = f();
    ON.set(false);
    READS.with_borrow_mut(|it| into.append(it));
    drop(off);
    out
}
