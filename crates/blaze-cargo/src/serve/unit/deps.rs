//! Dependency crates read from their images in the daemon's store (docs/v2/plan/design/frontend.md
//! item 5): [`hir_def::deps`] gives a front end each eligible crate's def map, lang items and
//! signatures from its image in place of resolving it, and writes the images the store lacks
//! once the crates are resolved. This is its store: the job's [`blaze_cas::Reader`] to read
//! from, in place, and the puts it asks for, handed to the loop, which alone writes the store.
//!
//! - A front end freshly lowered is served the crates of the units every run of whose closure
//!   is applied then, and each run applied serves those it made ready ([`serve`], adding to
//!   what was served): nothing a later run changes reaches them, and a crate that depends on a
//!   proc macro not built yet is not eligible.
//! - Its images are written once the head's next build lowers into the front end kept
//!   ([`store`]): every unit of the build it was kept from has compiled, so what each crate
//!   resolves to is memoized already.
//! - `BLAZE_DEP_IMAGES=check` resolves every crate served as well, and `verify` writes again
//!   what was read; either fails the build, once it has ended, on any difference from the
//!   images ([`differences`]): the daily differential run.
//! - The same store, through one adapter per database ([`Session`]), keeps what else is
//!   computed once for everyone: the dependencies' check outcomes and checked bodies
//!   ([`blaze_diagnostics::cache`]), their constants' values (`hir_ty::consteval::deps`) and
//!   global trait goals' answers (`hir_ty::next_solver::goal_store`). Each is served with the
//!   images and stored where they are.

use std::{
    panic,
    sync::{Arc, Mutex, OnceLock},
};

use base_db::{Crate, Db, Env};
use blaze_api::{ApiError, ServedCrate, ServedPart};
use blaze_cas::{Key, Kind};
use blaze_diagnostics::{Diag, Options, cache};
use blaze_ids::ContentId;
use hir_def::deps::{Held, ImageBuf, ImageStore, Mode, Served};
use hir_ty::next_solver::goal_store;
use load_cargo::v2::Lowered;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{Job, compile, graph};
use crate::serve::work::Put;

pub(crate) const IMAGES: &str = "BLAZE_DEP_IMAGES";

/// What the front end's images were served with, over every [`serve`] of it, kept with it
/// until they are stored.
pub(crate) struct Kept {
    pub(crate) served: Served,
    pub(crate) mode: Mode,
}

impl Kept {
    /// What a later [`serve`] of the same front end served, added: a crate's latest key holds.
    fn add(&mut self, more: Served) {
        let Served { keys, hits, stale } = more;
        self.served.keys.extend(keys);
        self.served.hits.extend(hits);
        self.served.hits.sort_unstable();
        self.served.hits.dedup();
        self.served.stale.extend(stale);
    }
}

/// A dependency crate a build checked, with the lint flags it was checked with and what the
/// check gave: its outcome and checked bodies are stored with the front end's images
/// ([`store`]).
pub(crate) struct Later {
    pub(crate) krate: Crate,
    pub(crate) flags: Vec<String>,
    pub(crate) diags: Vec<Diag>,
    /// Why its outcome can't be stored ([`cache::encode`]'s refusal), if it can't.
    pub(crate) refused: Option<String>,
}

/// The units of `job` that `waits` has ready: what a front end freshly lowered is served first.
pub(crate) fn ready(job: &Job, waits: &graph::Waits) -> Vec<u32> {
    (0..job.graph.len() as u32).filter(|&unit| waits.ready(unit)).collect() // alloc: setup, once per front end
}

/// The eligible crates of `units` (and their dependencies), served from the store through
/// `session`, adding to what `kept` holds of earlier serves of the front end: their images,
/// then their check outcomes and checked bodies, then their constants' values. Called with the
/// front end exclusive (no task reads it), once for the units ready when it is lowered and once
/// per batch of runs applied, with the units they made ready.
pub(crate) fn serve(
    lowered: &mut Lowered,
    job: &Job,
    units: &[u32],
    session: &mut Session,
    kept: &mut Option<Kept>,
) -> Result<(), ApiError> {
    let mode = mode_in(&job.input.config.invocation.env)?;
    let build = build()?;
    let served = lowered.serve_deps(units, build, &*session.images, mode).map_err(super::lower_error)?;
    debug_assert!(session.images.take_puts().is_empty(), "serving puts nothing");
    match kept {
        Some(kept) => kept.add(served),
        None => *kept = Some(Kept { served, mode }),
    }
    let Some(checks) = Arc::get_mut(&mut session.checks) else {
        return Err(ApiError::new("the dependencies' checks are served while a task still reads them", None));
    };
    let db = lowered.db();
    let flags = imaged(db, job, units.iter().map(|&unit| unit as usize))?;
    let options = Options::default();
    let asked: Vec<(Crate, cache::Inputs<'_>)> =
        flags.iter().map(|(krate, flags)| (*krate, cache::Inputs::of(flags, &options))).collect(); // alloc: once per serve
    cache::serve(lowered.served_db(), &asked, &*session.images, checks).map_err(|why| ApiError::new(why, None))?;
    let db = lowered.db();
    let consts = (0..job.graph.len())
        .filter_map(|unit| job.crates.krate(unit))
        .filter(|&krate| cache::image_key(db, krate).is_some())
        .map(|krate| (krate, checks.keys.get(&krate).copied()))
        .collect(); // alloc: once per serve, one entry per crate read from an image
    // dyn: the constants' store keeps it for the database's life: the same `Arc` each build
    let adapter: Arc<dyn ImageStore + Send + Sync> = session.images.clone(); // clone: an `Arc`, once per serve
    hir_ty::consteval::deps::set(lowered.served_db(), adapter, consts, mode);
    Ok(())
}

/// Each crate of `units` read from an image, once, with the lint flags its unit is checked
/// with, as `compile`'s `make` gives them to the check: what its outcome is keyed by.
fn imaged(db: &Db, job: &Job, units: impl Iterator<Item = usize>) -> Result<Vec<(Crate, Vec<String>)>, ApiError> {
    let mut crates: Vec<(Crate, usize)> = units
        .filter_map(|unit| Some((job.crates.krate(unit)?, unit)))
        .filter(|&(krate, _)| cache::image_key(db, krate).is_some())
        .collect(); // alloc: once per serve
    crates.sort_by_key(|it| it.0);
    crates.dedup_by_key(|it| it.0);
    crates.into_iter().map(|(krate, unit)| Ok((krate, compile::unit_lint_flags(job, unit)?))).collect() // alloc: as above
}

/// What a front end's database keeps of the store for its life: one adapter over it, its goal
/// store ([`goal_store::install`]) and the dependencies' check outcomes it was served.
/// Dropping it uninstalls the goal store.
pub(crate) struct Session {
    installed: goal_store::Installed,
    images: Arc<Images>,
    checks: Arc<cache::Served>,
    /// Why [`hir_def::deps::store`] wrote no image of each crate it last refused, for the
    /// report of the builds that read the database after ([`served`]).
    refused: FxHashMap<Crate, String>,
}

/// The session of `lowered`'s database over `store`, its goal store installed before any
/// query of it solves: a database made freshly, once.
pub(crate) fn session(lowered: &Lowered, store: &blaze_cas::Reader) -> Result<Session, ApiError> {
    let images = Arc::new(Images::new(store.clone())); // alloc: setup, once per database; clone: an `Arc`
    // dyn: the goal store keeps both for the database's life; called once per database
    let adapter: Arc<dyn ImageStore + Send + Sync> = images.clone(); // clone: an `Arc`, once per database
    // dyn: hir-ty's goal store takes its facts as `dyn` (blaze-items depends on hir-ty, which can't name `Goals`); made once per database
    let facts: Arc<dyn goal_store::GoalFacts> = Arc::new(blaze_items::goals::Goals); // alloc: setup, once per database
    let installed = goal_store::install(lowered.db(), adapter, facts, build()?)
        .map_err(|why| ApiError::new(format!("the persistent goal cache: {why}"), None))?; // alloc: error path
    let checks = Arc::new(cache::Served::default()); // alloc: setup, once per database
    Ok(Session { installed, images, checks, refused: FxHashMap::default() })
}

impl Session {
    /// The dependencies' check outcomes the database was served, for a job's units to read.
    pub(crate) fn checks(&self) -> Arc<cache::Served> {
        Arc::clone(&self.checks)
    }
}

/// What the job's `BLAZE_DEP_IMAGES` asks done with an image: unset serves it, `check`
/// resolves its crate as well, `verify` writes again what was read; any other value is an
/// error naming it.
fn mode_in(env: &[(String, String)]) -> Result<Mode, ApiError> {
    match env.iter().find(|(name, _)| name == IMAGES).map(|(_, value)| value.as_str()) {
        None => Ok(Mode::Serve),
        Some("check") => Ok(Mode::Check),
        Some("verify") => Ok(Mode::Verify),
        Some(value) => Err(ApiError::new(
            format!(
                "{IMAGES}={value} is not a value blaze takes: `check` resolves each dependency read from its \
                 image as well, `verify` writes again what was read, unset reads it"
            ),
            None,
        )),
    }
}

/// Once a build has ended: under `check` or `verify`, each difference its crates showed from
/// their images, as the build's error. Nothing is recorded when serving.
pub(crate) fn differences(kept: &Kept) -> Result<(), ApiError> {
    if kept.mode == Mode::Serve {
        return Ok(());
    }
    let found = hir_def::nameres::frozen::differences();
    if found.is_empty() {
        return Ok(());
    }
    let what = format!(
        "{IMAGES}: {} dependency images differ from what their crates resolve to:\n{}",
        found.len(),
        found.join("\n")
    );
    Err(ApiError::new(what, None))
}

/// What the build a front end was kept from leaves to keep, written from what `lowered`
/// computed, as the puts for the loop to make: the images of the crates `kept` keyed and found
/// none of (a crate whose image would not cover every input is refused by
/// [`hir_def::deps::store`] and resolved again in each build); the outcomes and checked bodies
/// of the dependency crates it checked (`later`, their paths named from `root`), after their
/// dependencies'; the constants' values they evaluated; the goals offered. Called when the
/// head's next build lowers into the front end, which no task reads then.
pub(crate) fn store(
    lowered: &Lowered,
    kept: Option<Kept>,
    session: &mut Session,
    later: &[Later],
    root: &str,
) -> Result<Vec<Put>, ApiError> {
    let images = &*session.images;
    let stored = match &kept {
        Some(kept) => Some(lowered.store_deps(&kept.served, images).map_err(super::lower_error)?),
        None => None,
    };
    let db = lowered.db();
    let options = Options::default();
    let checked: Vec<cache::Checked<'_>> = later
        .iter()
        .filter_map(|it| {
            // None: its image was refused, and its outcome is for the same reason.
            let image = cache::image_key(db, it.krate).or_else(|| stored.as_ref()?.filed.get(&it.krate).copied())?;
            Some(cache::Checked { krate: it.krate, image, inputs: cache::Inputs::of(&it.flags, &options), diags: &it.diags })
        })
        .collect(); // alloc: once per front end stored, one per crate checked
    let error = |why: String| ApiError::new(why, None);
    let kept_checks = cache::store(db, &checked, &session.checks, root, images).map_err(error)?;
    let mut bases = session.checks.keys.clone(); // clone: once per front end stored
    bases.extend(kept_checks.keys);
    hir_ty::consteval::deps::drain(db, &bases).map_err(error)?;
    session.installed.flush().map_err(|why| error(format!("the persistent goal cache: {why}")))?; // alloc: error path
    let puts = images.take_puts();
    if let Some(stored) = &stored {
        for krate in &stored.written {
            session.refused.remove(krate);
        }
        // alloc: once per front end stored, one per crate refused an image
        session.refused.extend(stored.refused.iter().map(|(krate, why)| (*krate, why.to_string())));
    }
    Ok(puts)
}

/// What `job`'s build was served of each of its library crates that is no workspace member,
/// once each, sorted by cargo package name ([`blaze_api::Worked::served`]): its check outcome
/// and its bodies each served from `session`'s store, checked this build, or refused, with
/// why. A crate read from no image is refused both, with the reason [`hir_def::deps::store`]
/// last gave for it, or [`cache::Refused::NoImage`]'s when it has given none.
pub(crate) fn served(db: &Db, job: &Job, session: &Session) -> Result<Vec<ServedCrate>, ApiError> {
    let later = job.later.lock().unwrap_or_else(|it| it.into_inner());
    // alloc: once per build, one entry per dependency crate checked
    let outcomes: FxHashMap<Crate, Option<&str>> = later.iter().map(|it| (it.krate, it.refused.as_deref())).collect();
    let mut seen = FxHashSet::default();
    let mut lines = Vec::new(); // alloc: once per build, one line per dependency library
    for unit in 0..job.graph.len() {
        let Some(krate) = job.crates.krate(unit) else { continue };
        let u = job.unit(unit)?;
        if !u.target.is_lib() || !seen.insert(krate) {
            continue;
        }
        let package = job.package(u)?;
        if package.is_member {
            continue;
        }
        let package = package.name.clone(); // clone: once per line
        let line = match cache::image_key(db, krate) {
            None => {
                let why = match session.refused.get(&krate) {
                    Some(why) => why.clone(), // clone: once per line refused
                    None => cache::Refused::NoImage.to_string(),
                };
                ServedCrate { package, outcome: ServedPart::Refused, bodies: ServedPart::Refused, why: Some(why) }
            }
            Some(_) => {
                let refused = outcomes.get(&krate).copied().flatten();
                let outcome = match (session.checks.has(krate), refused) {
                    (true, _) => ServedPart::Served,
                    (false, Some(_)) => ServedPart::Refused,
                    (false, None) => ServedPart::Checked,
                };
                let bodies = match session.checks.keys.contains_key(&krate) {
                    true => ServedPart::Served,
                    false => ServedPart::Checked,
                };
                ServedCrate { package, outcome, bodies, why: refused.map(str::to_owned) }
            }
        };
        lines.push(line);
    }
    lines.sort_by(|a, b| a.package.cmp(&b.package));
    Ok(lines)
}

/// The identity of the blaze reading and writing images: its own code with the pinned nightly
/// it compiles against, which decides what a crate resolves to. Never a constant: a fixed
/// name-resolution bug must not go on serving the def maps the bug made.
fn build() -> Result<ContentId, ApiError> {
    static BUILD: OnceLock<Result<ContentId, String>> = OnceLock::new();
    let worked_out = BUILD.get_or_init(|| {
        let host = crate::cargo::toolchain::host()
            .ok_or("blaze has no target triple for this machine's architecture and system")?;
        hir_ty_codegen::v2::toolchain(host, &[]).map(|it| it.id)
    });
    worked_out.clone().map_err(|why| ApiError::new(format!("the identity dependency images are keyed by: {why}"), None))
}

/// The store as [`hir_def::deps`] and the goal store read and write it.
struct Images {
    store: blaze_cas::Reader,
    /// What it asked to keep, in the order asked; a write path, once per crate it images and
    /// per goal a flush writes.
    puts: Mutex<Vec<Put>>,
}

impl Images {
    fn new(store: blaze_cas::Reader) -> Images {
        Images { store, puts: Mutex::new(Vec::new()) }
    }

    /// What it was asked to keep since the last take, in the order asked.
    fn take_puts(&self) -> Vec<Put> {
        std::mem::take(&mut *self.puts.lock().unwrap_or_else(|it| it.into_inner()))
    }
}

/// A value of the store held in place past the read ([`blaze_cas::Reader::pinned`]): verified
/// against its content hash, at a 16-byte boundary, and unmoved while it lives.
struct InStore(blaze_cas::Pinned);

impl Held for InStore {
    #[inline]
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl ImageStore for Images {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        let guard = blaze_cas::enter();
        match self.store.pinned(&guard, &Key(*key)) {
            // dyn: hir-def's holder of one image, once per crate per front end
            Ok(Some(pinned)) => Ok(Some(Arc::new(InStore(pinned)))),
            Ok(None) => Ok(None),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        let guard = blaze_cas::enter();
        match self.store.get(&guard, &Key(key)) {
            Ok(Some(entry)) if entry.bytes == value.as_slice() => return Ok(()),
            Ok(Some(_)) => return Err(format!("the store holds other bytes under {key:?}")),
            Ok(None) => {}
            Err(corrupt) => return Err(corrupt.to_string()),
        }
        let mut puts = self.puts.lock().unwrap_or_else(|it| it.into_inner());
        match puts.iter().find(|it| it.key == Key(key)) {
            Some(put) if put.bytes == value => {}
            Some(_) => return Err(format!("two images were made under {key:?}")),
            None => puts.push(Put { key: Key(key), kind: Kind::Query, bytes: value }),
        }
        Ok(())
    }

    fn reads_now(&self, env: &Env, shape: &[u8]) -> Result<ContentId, String> {
        blaze_expand::reads_now(env, shape)
    }

    // dyn: as `ImageStore::for_each`, one call per batch of crates
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        on_pool(count, task);
    }
}

/// `task(at)` for each `at` in `0..count` on the shared pool, each in a frame of its own, as
/// [`base_db::run_on_pool`] runs a batch: a task that would wait on a query its own thread
/// runs below it runs again in a round of its own, and the first other panic goes on from here
/// once its round is over.
// dyn: as `ImageStore::for_each`
fn on_pool(count: usize, task: &(dyn Fn(usize) + Sync)) {
    let task = |at: usize| blaze_mem::frame::task(blaze_mem::Tag::Query, |_| task(at));
    let pool = blaze_graph::exec::shared();
    let near = blaze_graph::pool::group();
    let panicked = Mutex::new(None);
    let again = Mutex::new(Vec::new());
    let mut round: Vec<usize> = (0..count).collect(); // alloc: setup, once per batch of crates
    while !round.is_empty() {
        pool.scope(|scope| {
            for &at in &round {
                let (panicked, again) = (&panicked, &again);
                scope.submit(near, move || task(at), move |ran| {
                    let Err(panic) = ran else { return };
                    if matches!(panic.downcast_ref::<salsa::Cancelled>(), Some(salsa::Cancelled::WouldWaitOnItself)) {
                        again.lock().unwrap_or_else(|it| it.into_inner()).push(at);
                    } else {
                        panicked.lock().unwrap_or_else(|it| it.into_inner()).get_or_insert(panic);
                    }
                });
            }
        });
        round.clear();
        std::mem::swap(&mut round, &mut *again.lock().unwrap_or_else(|it| it.into_inner()));
        if panicked.lock().unwrap_or_else(|it| it.into_inner()).is_some() {
            break;
        }
    }
    if let Some(panic) = panicked.into_inner().unwrap_or_else(|it| it.into_inner()) {
        panic::resume_unwind(panic);
    }
}

#[cfg(test)]
mod tests;
