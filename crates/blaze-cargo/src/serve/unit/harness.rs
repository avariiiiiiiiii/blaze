//! A test crate's harness written into its root file before its unit is identified
//! ([`write`]): the `main` and the description of each test that rustc's `--test` adds, with
//! usercode's fork server, as [`HirFrontend::harness`] writes them. The harness is in the root's
//! text and appended module, so the unit's identity and record key cover it, and a harness of
//! another mode is another text: the unit is stale.

#![forbid(unsafe_code)]

use std::sync::Arc;

use blaze_api::ApiError;
use blaze_ids::radix;
use blaze_units::schema::Mode;
use hir_ty_codegen::v2::HirFrontend;
use load_cargo::v2::{Appended, LowerError, Lowered};

use super::{Job, codegen, graph::Waits, lower_error};

macro_rules! module {
    () => {
        "__blaze_test_harness"
    };
}

/// The module [`HirFrontend::harness`] writes, by its name in the crate's root: what the
/// front end keeps of a root that `#![cfg]` takes away.
const MODULE: &str = module!();

/// How the harness opens [`MODULE`]: a harness that doesn't is refused, so the appended
/// module is never named for one it lacks.
const OPENS: &str = concat!("pub mod ", module!(), " {");

/// The harness of each of `units` built with `cfg(test)` (a test, a check of tests) written
/// into its crate's root: the root's text on disk, then the harness. A test with
/// `harness = false` has none and its root is left as it is. Called on the loop's writer
/// as these units become ready, before they are identified, so what they are identified by
/// has the harness.
///
/// A crate that reaches a proc macro whose expanders are not loaded yet is left: what its
/// attributes expand to (a `#[tokio::test]`'s `#[test]`) is not listed until they are, and
/// [`macros_applied`] writes it then. Its unit's identity was set before; the harness is made
/// from what that identity covers and the unit's mode and panic strategy, which its record key
/// covers, so the key stays true.
///
/// Each takes the panic strategy of its profile and the mode of its codegen policy
/// ([`codegen::policy`]), which a unit's build is compiled with. Two units of one root that
/// want different harnesses are refused naming both ([`Lowered::append_roots`]), as is a
/// harness that can't be made, naming its crate.
pub(super) fn write(lowered: &mut Lowered, job: &Job, units: impl IntoIterator<Item = u32>) -> Result<(), ApiError> {
    let paths = Arc::clone(lowered.paths());
    let tested = units.into_iter().filter(|&unit| tested(job, unit as usize));
    let made = lowered.append_roots(tested, |db, unit, krate| {
        if !load_cargo::v2::macros_loaded(db, krate) {
            return Ok(None);
        }
        let at = || match job.unit(unit) {
            // alloc: an error's place
            Ok(it) => format!("`{}` of `{}`", it.target.name, it.pkg_id),
            Err(error) => error.what,
        };
        let refused = |what: String| LowerError { what, at: Some(at()) };
        let toolchain = codegen::toolchain(job, unit).map_err(refused)?;
        let policy = codegen::policy(job, unit, toolchain).map_err(refused)?;
        let frontend = codegen::frontend_over(job, db, &paths, &job.mounted);
        let harness = frontend.harness(krate, policy.profile.panic, policy.mode, blaze_usercode::test::fork_server);
        let Some(harness) = harness.map_err(|it| refused(it.what))? else { return Ok(None) };
        if !harness.contains(OPENS) {
            // alloc: the error's message
            return Err(refused(format!("the test harness blaze wrote has no module `{MODULE}`")));
        }
        let on_disk = HirFrontend::on_disk(db.file_text(krate.data(db).root_file_id).text(db));
        // alloc: once a test crate a lowering, the root's text the front end is given
        let mut text = String::with_capacity(on_disk.len() + harness.len());
        text.push_str(on_disk);
        text.push_str(&harness);
        Ok(Some(Appended { text, module: MODULE }))
    });
    made.map(drop).map_err(lower_error)
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

/// Whether `unit`'s crate is built with `cfg(test)`, as load-cargo makes it: a test, or a check
/// of one (`cargo check --tests`).
fn tested(job: &Job, unit: usize) -> bool {
    job.unit(unit).is_ok_and(|it| it.mode == Mode::Test) || job.plan(unit).is_ok_and(|it| it.checked_as_test)
}
