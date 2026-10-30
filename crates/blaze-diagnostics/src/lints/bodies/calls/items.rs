//! rustc's late lint passes of calls of std's `drop`, `forget`, `from_utf8` and atomics.
//!
//! - `drop_forget_useless`: `DropForgetUseless` (`dropping_references`, `dropping_copy_types`,
//!   `forgetting_references`, `forgetting_copy_types`, `undropped_manually_drops`).
//! - `invalid_from_utf8`: `InvalidFromUtf8` (`invalid_from_utf8_unchecked`,
//!   `invalid_from_utf8`).
//! - `invalid_atomic_ordering`: `InvalidAtomicOrdering` (`invalid_atomic_ordering`).
//!
//! Follows rustc's `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedLateLintModPass`) (MIT or
//! Apache-2.0).

mod drop_forget_useless;
mod invalid_atomic_ordering;
mod invalid_from_utf8;

use base_db::Db;
use hir_def::FunctionId;
use rustc_hash::FxHashMap;

use crate::lints::bodies::types::util;

/// rustc's late lint passes of calls of std's `drop`, `forget`, `from_utf8` and atomics.
pub(super) fn passes() -> Vec<Box<dyn crate::lints::bodies::LatePass>> {
    vec![
        Box::new(drop_forget_useless::DropForgetUseless::default()),
        Box::new(invalid_from_utf8::InvalidFromUtf8::default()),
        Box::new(invalid_atomic_ordering::InvalidAtomicOrdering::default()),
    ]
}

/// Which of the diagnostic items a pass looks for each function called is, read from its
/// attributes once per function.
struct FnItems {
    known: &'static [&'static str],
    seen: FxHashMap<FunctionId, Option<&'static str>>,
}

impl FnItems {
    fn new(known: &'static [&'static str]) -> FnItems {
        FnItems { known, seen: FxHashMap::default() }
    }

    /// rustc's `get_diagnostic_name` of `def`, when it is one of those looked for.
    fn of(&mut self, db: &Db, def: FunctionId) -> Option<&'static str> {
        let known = self.known;
        *self.seen.entry(def).or_insert_with(|| {
            let name = util::fn_item(db, def)?;
            known.iter().copied().find(|it| *it == name.as_str())
        })
    }
}
