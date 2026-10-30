//! What `blaze check` checks (#1293): every item and every body of the workspace's crates, as
//! rustc checks every crate it compiles, with their borrow checks.
//!
//! Nothing here asks for a dependency's bodies. Of a dependency, checking a workspace body
//! infers only what that body reaches through what a body leaks into its interface: an opaque
//! type's hidden type and its auto traits, a constant it evaluates, a `const fn` it calls in a
//! const context. A build reaches the rest of a dependency's bodies only through codegen of the
//! instances it compiles.

use hir_def::nameres::crate_def_map;

use crate::{
    AnyDiagnostic, Crate, DefWithBody, db::HirDatabase, diagnostics::DiagnosticsCollector,
};

/// The crates `blaze check` checks: the workspace's members.
pub fn workspace_crates(db: &dyn HirDatabase) -> Vec<Crate> {
    Crate::all(db).into_iter().filter(|it| it.origin(db).is_local()).collect()
}

/// The diagnostics of every item and every body of `krate`, borrow checks included; with
/// `bodies`, of the bodies it accepts only (a driver that knows which bodies an edit left as
/// they were checked).
pub fn check_crate<'db>(
    db: &'db dyn HirDatabase,
    krate: Crate,
    style_lints: bool,
    bodies: Option<&dyn Fn(DefWithBody) -> bool>,
) -> Vec<AnyDiagnostic<'db>> {
    let mut acc = Vec::new();
    let def_map = crate_def_map(db, krate.id);
    for (module, data) in def_map.modules(db) {
        // An inline module is checked with the module it is written in.
        if data.origin.is_inline() {
            continue;
        }
        DiagnosticsCollector::collect(db, module, &mut acc, style_lints, bodies);
    }
    acc
}

/// [`check_crate`] of each of `crates`, in their order.
pub fn check_crates<'db>(
    db: &'db dyn HirDatabase,
    crates: &[Crate],
    style_lints: bool,
) -> Vec<(Crate, Vec<AnyDiagnostic<'db>>)> {
    crates.iter().map(|&krate| (krate, check_crate(db, krate, style_lints, None))).collect()
}

/// [`check_crates`] of [`workspace_crates`].
pub fn check_workspace(
    db: &dyn HirDatabase,
    style_lints: bool,
) -> Vec<(Crate, Vec<AnyDiagnostic<'_>>)> {
    check_crates(db, &workspace_crates(db), style_lints)
}
