//! What the compiler appends to a crate's root file, written into it before its unit is
//! identified ([`write`]): a test crate's harness, the `main` and the description of each test
//! that rustc's `--test` adds, with usercode's fork server, as [`HirFrontend::harness`] writes
//! them; and a proc macro's list of its macros, which rustc keeps in the crate's metadata and
//! blaze in a module the loader reads ([`HirFrontend::proc_macro_list`]). Either is in the
//! root's text and appended module, so the unit's identity and record key cover it, and a
//! harness of another mode is another text: the unit is stale.

#![forbid(unsafe_code)]

use std::sync::Arc;

use base_db::{Crate, Db};
use blaze_api::ApiError;
use blaze_ids::radix;
use blaze_units::schema::Mode;
use hir_ty_codegen::v2::{HARNESS_MODULE, HirFrontend, proc_macro_list};
use blaze_lower::{Appended, LowerError, Lowered};

use super::{Job, codegen, graph::Waits, lower_error};

/// The harness of each of `units` built with `cfg(test)` (a test, a check of tests), and the
/// list of each proc macro's macros, written into its crate's root: the root's text on disk,
/// then the module. A test with `harness = false` has none and its root is left as it is.
/// Called on the loop's writer as these units become ready, before they are identified, so
/// what they are identified by has the module.
///
/// A crate that reaches a proc macro whose expanders are not loaded yet is left: what its
/// attributes expand to (a `#[tokio::test]`'s `#[test]`, a function of the root) is not listed
/// until they are, and [`macros_applied`] writes it then. Its unit's identity was set before;
/// the module is made from what that identity covers and the unit's mode and panic strategy,
/// which its record key covers, so the key stays true.
///
/// Each harness takes the panic strategy of its profile and the mode of its codegen policy
/// ([`codegen::policy`]), which a unit's build is compiled with. Two units of one root that
/// want different texts (a proc macro and its lib's test, built in one job) are refused naming
/// both ([`Lowered::append_roots`]), as is a module that can't be made, naming its crate.
pub(super) fn write(lowered: &mut Lowered, job: &Job, units: impl IntoIterator<Item = u32>) -> Result<(), ApiError> {
    let paths = Arc::clone(lowered.paths());
    let appended = units.into_iter().filter(|&unit| tested(job, unit as usize) || job.crates.is_proc_macro(unit as usize));
    let made = lowered.append_roots(appended, |db, unit, krate| {
        if !blaze_lower::macros_loaded(db, krate) {
            return Ok(None);
        }
        let at = || match job.unit(unit) {
            // alloc: an error's place
            Ok(it) => format!("`{}` of `{}`", it.target.name, it.pkg_id),
            Err(error) => error.what,
        };
        let refused = |what: String| LowerError { what, at: Some(at()) };
        if job.crates.is_proc_macro(unit) {
            let frontend = codegen::frontend_over(job, db, &paths, &job.mounted);
            return proc_macro_root(db, &frontend, krate).map(Some).map_err(refused);
        }
        let toolchain = codegen::toolchain(job, unit).map_err(refused)?;
        let policy = codegen::policy(job, unit, toolchain).map_err(refused)?;
        let frontend = codegen::frontend_over(job, db, &paths, &job.mounted);
        let harness = frontend.harness(krate, policy.profile.panic, policy.mode, blaze_usercode::test::fork_server);
        let Some(harness) = harness.map_err(|it| refused(it.what))? else { return Ok(None) };
        let on_disk = HirFrontend::on_disk(db.file_text(krate.data(db).root_file_id).text(db));
        // alloc: once a test crate a lowering, the root's text the front end is given
        let mut text = String::with_capacity(on_disk.len() + harness.len());
        text.push_str(on_disk);
        text.push_str(&harness);
        // The module the harness is written in, by its name in the crate's root: what the
        // front end keeps of a root that `#![cfg]` takes away.
        Ok(Some(Appended { text, module: HARNESS_MODULE }))
    });
    made.map(drop).map_err(lower_error)
}

/// The root of `krate`, a proc macro's crate, as it is compiled: its text on disk, then the
/// list of its macros, which the code generator's roots of a proc macro require and the
/// loader of blaze's proc-macro server reads.
pub(super) fn proc_macro_root(db: &Db, frontend: &HirFrontend<'_>, krate: Crate) -> Result<Appended, String> {
    let list = frontend.proc_macro_list(krate).map_err(|it| it.what)?;
    let on_disk = HirFrontend::on_disk(db.file_text(krate.data(db).root_file_id).text(db));
    // alloc: once a proc macro a lowering, the root's text the front end is given
    let mut text = String::with_capacity(on_disk.len() + list.len());
    text.push_str(on_disk);
    text.push_str(&list);
    Ok(Appended { text, module: proc_macro_list::MODULE })
}

/// The harnesses the proc macros `loaded` (their units, just applied) complete: of every unit
/// that is ready and reaches one of them through its dependents ([`write`]). A unit not ready
/// yet is written when it is.
pub(super) fn macros_applied(lowered: &mut Lowered, job: &Job, waits: &Waits, loaded: &[u32]) -> Result<(), ApiError> {
    if loaded.is_empty() {
        return Ok(());
    }
    // alloc: once an apply that loaded proc macros, the units they reach
    let mut seen = vec![false; job.graph.len()];
    let mut stack: Vec<u32> = loaded.to_vec(); // alloc: as `seen`
    let mut reached = Vec::new(); // alloc: as `seen`
    while let Some(at) = stack.pop() {
        for &up in job.graph.dependents(at) {
            if !std::mem::replace(&mut seen[up as usize], true) {
                stack.push(up);
                if waits.ready(up) {
                    reached.push(up);
                }
            }
        }
    }
    radix::sort_by_key(&mut reached, |&it| u64::from(it));
    write(lowered, job, reached)
}

/// Whether `unit`'s crate is built with `cfg(test)`, as blaze-lower makes it: a test, or a check
/// of one (`cargo check --tests`).
fn tested(job: &Job, unit: usize) -> bool {
    job.unit(unit).is_ok_and(|it| it.mode == Mode::Test) || job.plan(unit).is_ok_and(|it| it.checked_as_test)
}
