//! What rustc lints of names and items: name resolution's lints, the stability checks, the
//! dead-code pass and the late lint passes over items, each found as rustc's pass finds it.
//!
//! - `imports`: what `rustc_resolve` lints (`unused_imports`, `unused_macros`,
//!   `unused_qualifications`, `ambiguous_glob_imports`, ..).
//! - `stability`: `deprecated` and the other stability lints (`rustc_middle/src/middle/
//!   stability.rs`, `rustc_passes/src/stability.rs`, typeck's method calls).
//! - `dead`: `dead_code` (`rustc_passes/src/dead.rs`).
//! - `items`: the lints of items `rustc_hir_analysis` gives, and `rustc_lint`'s late passes
//!   over items (`non_upper_case_globals`, `missing_docs`, `improper_ctypes`, ..).
//!
//! Follows `compiler/rustc_interface/src/passes.rs` (`run_required_analyses`, `analysis`,
//! `misc_checking_3`) for when each is given (MIT or Apache-2.0).

mod dead;
mod imports;
mod items;
mod stability;

use crate::{cx::Cx, lower::Found};

/// What name resolution and the stability checks lint, given whatever else failed.
pub(crate) fn before_abort(cx: &Cx<'_, '_>) -> Vec<Found> {
    let mut found = imports::lints(cx);
    found.extend(stability::lints(cx));
    found.extend(items::before_abort(cx));
    found
}

/// What rustc's dead-code pass and its late lint passes of items lint, run only when nothing
/// failed, in `misc_checking_3`'s order: private types in public interfaces, deathness, the
/// late lint passes, then the extern declarations that clash.
pub(crate) fn after_abort(cx: &Cx<'_, '_>) -> Vec<Found> {
    let walk = items::Walk::of(cx);
    let mut found = items::private_in_public(cx, &walk);
    found.extend(dead::lints(cx));
    found.extend(items::after_abort(cx, &walk));
    found.extend(items::clashing_extern_declarations(cx, &walk));
    found
}
