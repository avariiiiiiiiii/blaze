//! What `blaze check` checks (#1293): every item and every body of the workspace's crates, as
//! rustc checks every crate it compiles, with their borrow checks.
//!
//! Nothing here asks for a dependency's bodies. Of a dependency, checking a workspace body
//! infers only what that body reaches through what a body leaks into its interface: an opaque
//! type's hidden type and its auto traits, a constant it evaluates, a `const fn` it calls in a
//! const context. A build reaches the rest of a dependency's bodies only through codegen of the
//! instances it compiles.
//!
//! The unit of work is a file module with the inline modules written in it ([`check_module`]).
//! The modules of a check go to the database as one batch ([`SourceDatabase::run_all`]), which
//! runs them as plane tasks over the one database; each task's diagnostics go in its module's
//! slot, and the result is read in the def maps' order, so it is the same whatever order the
//! tasks ran in.
//!
//! The technique to match is rustc's parallel front end, `par_hir_for_each_module` on rayon's
//! work stealing: one module per task. Here a run of small modules is one task, so no task
//! costs less to schedule than it does to run, a slot is written once by a compare-and-swap
//! (no lock), and unlike rustc's the diagnostics come out in one order whatever the threads.

use std::{ptr, sync::OnceLock};

use base_db::SourceDatabase;
use hir_def::nameres::crate_def_map;

use crate::{
    AnyDiagnostic, Crate, DefWithBody, Module, db::HirDatabase, diagnostics::DiagnosticsCollector,
};

/// Which bodies a check asks for: all of them when `None`.
pub type Bodies<'a> = Option<&'a (dyn Fn(DefWithBody) -> bool + Sync)>;

/// The items a task takes at least, but for the last: a run of smaller modules is one task.
/// The tests' fixtures are a few items a module, which would otherwise be one task.
const ITEMS_PER_TASK: usize = if cfg!(test) { 4 } else { 32 };

/// The crates `blaze check` checks: the workspace's members.
pub fn workspace_crates(db: &dyn HirDatabase) -> Vec<Crate> {
    Crate::all(db).into_iter().filter(|it| it.origin(db).is_local()).collect()
}

/// The modules of `krate` a check takes one at a time: every module but the inline ones, which
/// are checked with the module they are written in, in the def map's order.
pub fn file_modules(db: &dyn HirDatabase, krate: Crate) -> Vec<Module> {
    crate_def_map(db, krate.id)
        .modules(db)
        .filter(|(_, data)| !data.origin.is_inline())
        .map(|(id, _)| Module { id })
        .collect()
}

/// The diagnostics of `module`, of the inline modules written in it, and of every item and
/// body declared in them, borrow checks included; with `bodies`, of the bodies it accepts only
/// (a driver that knows which bodies an edit left as they were checked). The work of one plane
/// task: it asks queries of the database and keeps nothing.
pub fn check_module<'db>(
    db: &'db dyn HirDatabase,
    module: Module,
    style_lints: bool,
    bodies: Bodies<'_>,
) -> Vec<AnyDiagnostic<'db>> {
    let mut acc = Vec::new();
    let bodies = bodies.map(|it| it as &dyn Fn(DefWithBody) -> bool);
    DiagnosticsCollector::collect(db, module.id, &mut acc, style_lints, bodies);
    acc
}

/// [`check_module`] of each of [`file_modules`] of `krate`, in their order.
pub fn check_crate<'db>(
    db: &'db dyn HirDatabase,
    krate: Crate,
    style_lints: bool,
    bodies: Bodies<'_>,
) -> Vec<AnyDiagnostic<'db>> {
    check_modules(db, &file_modules(db, krate), style_lints, bodies).into_iter().flatten().collect()
}

/// [`check_crate`] of each of `crates`, in their order, their modules all in one batch.
pub fn check_crates<'db>(
    db: &'db dyn HirDatabase,
    crates: &[Crate],
    style_lints: bool,
) -> Vec<(Crate, Vec<AnyDiagnostic<'db>>)> {
    let per_crate: Vec<Vec<Module>> = crates.iter().map(|&it| file_modules(db, it)).collect();
    let modules: Vec<Module> = per_crate.iter().flatten().copied().collect();
    let mut found = check_modules(db, &modules, style_lints, None).into_iter();
    crates
        .iter()
        .zip(&per_crate)
        .map(|(&krate, of)| (krate, found.by_ref().take(of.len()).flatten().collect()))
        .collect()
}

/// [`check_crates`] of [`workspace_crates`].
pub fn check_workspace(
    db: &dyn HirDatabase,
    style_lints: bool,
) -> Vec<(Crate, Vec<AnyDiagnostic<'_>>)> {
    check_crates(db, &workspace_crates(db), style_lints)
}

/// [`check_module`] of each of `modules` as one batch of the database's tasks; the diagnostics
/// of each, in the order of `modules`.
fn check_modules<'db>(
    db: &'db dyn HirDatabase,
    modules: &[Module],
    style_lints: bool,
    bodies: Bodies<'_>,
) -> Vec<Vec<AnyDiagnostic<'db>>> {
    let starts = tasks(db, modules);
    if starts.len() < 2 {
        return modules.iter().map(|&it| check_module(db, it, style_lints, bodies)).collect();
    }
    let slots: Vec<OnceLock<Vec<AnyDiagnostic<'db>>>> =
        modules.iter().map(|_| OnceLock::new()).collect();
    HirDatabase::zalsa_register_downcaster(db);
    let home = ptr::from_ref(db).cast::<()>().addr();
    db.run_all(starts.len(), &|task_db, task| {
        let task_db = hir_db(task_db);
        assert_eq!(
            ptr::from_ref(task_db).cast::<()>().addr(),
            home,
            "a task of a check ran on another database than the one checked"
        );
        // SAFETY: `task_db` is the object `db` points at (checked above), which is borrowed for
        // `'db` by this call, and `run_all` returns only once every task has run. `'db` brands
        // the diagnostics as that database's, which this one is.
        let task_db: &'db dyn HirDatabase = unsafe { &*ptr::from_ref(task_db) };
        let end = starts.get(task + 1).map_or(modules.len(), |&it| it as usize);
        for at in starts[task] as usize..end {
            let found = check_module(task_db, modules[at], style_lints, bodies);
            assert!(slots[at].set(found).is_ok(), "a module of a check was checked twice");
        }
    });
    slots
        .into_iter()
        .map(|it| it.into_inner().expect("`run_all` returned before a task of the check ran"))
        .collect()
}

/// Where each task of `modules` starts: a run of modules, together of [`ITEMS_PER_TASK`]
/// items or more but for the last, in order.
pub(crate) fn tasks(db: &dyn HirDatabase, modules: &[Module]) -> Vec<u32> {
    let mut starts = Vec::with_capacity(modules.len());
    let mut items = ITEMS_PER_TASK;
    for (at, module) in modules.iter().enumerate() {
        if items >= ITEMS_PER_TASK {
            starts.push(at as u32);
            items = 0;
        }
        let scope = module.id.def_map(db).module(db, module.id).scope(db);
        items += 1 + scope.declarations().count() + scope.impls().len();
    }
    starts
}

/// `db` as the `HirDatabase` it is, once [`HirDatabase::zalsa_register_downcaster`] was called on
/// it.
fn hir_db(db: &dyn SourceDatabase) -> &(dyn HirDatabase + 'static) {
    let caster = salsa::plumbing::views(db).downcaster_for::<dyn HirDatabase>();
    // SAFETY: a database's views are made for that database's own type.
    unsafe { caster.downcast_unchecked(db.into()) }
}
