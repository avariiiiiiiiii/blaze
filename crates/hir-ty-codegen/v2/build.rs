//! What a v2 unit's [`Spec`] decides of its IR, for the IR builder: v1 reads these from the
//! crate's cfg and the process's environment, a v2 unit from its spec alone.
//!
//! The IR builder reads [`building`] when it starts a function (`Recorder::new`): with
//! `Some`, the compiling crate's `-C debug-assertions` (what `ub_checks()` folds to) and panic
//! strategy are these, and it builds in no callee's body, since a body built in would be code
//! the unit's references and key don't name.

use std::cell::Cell;

use blaze_codegen::settings::{Panic, Spec};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Build {
    /// `-C debug-assertions` of the compiling crate (`Recorder::debug_assertions`).
    pub(crate) debug_assertions: bool,
    /// `panic = "abort"`: no landing pads.
    pub(crate) aborts: bool,
}

impl Build {
    pub(crate) fn of(spec: &Spec) -> Self {
        Build { debug_assertions: spec.debug_assertions, aborts: spec.panic == Panic::Abort }
    }
}

thread_local! {
    static BUILDING: Cell<Option<Build>> = const { Cell::new(None) };
    static READ: Cell<bool> = const { Cell::new(false) };
}

/// The settings of the v2 unit whose IR the thread is building; `None` outside one.
pub(crate) fn building() -> Option<Build> {
    READ.set(true);
    BUILDING.get()
}

/// Runs `work`, which builds one function's IR, with `build` as what [`building`] says. An
/// IR builder that never asked would have built the function as v1 does: that is an error.
pub(crate) fn with<T>(build: Build, work: impl FnOnce() -> T) -> Result<T, String> {
    let held = (BUILDING.replace(Some(build)), READ.replace(false));
    let out = crate::tier::plain(work);
    let read = READ.replace(held.1);
    BUILDING.set(held.0);
    if !read {
        return Err("the IR builder did not read the unit's settings (`v2::build::building`), \
                    so it built the function with the crate's"
            .to_owned());
    }
    Ok(out)
}
