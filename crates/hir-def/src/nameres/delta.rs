//! Name resolution maintained by deltas (docs/v2/plan/design/dbsp.md §3–6, §9): maintainer
//! `100`, which every write to the database ends with. It keeps each crate's resolution in its
//! own state ([`state`]), reads what a write changed as a delta of round-0 logs ([`input`]),
//! runs again only the groups that delta and their readers reach ([`run`]), and publishes per
//! module ([`publish`]): `crate_local_def_map` of a maintained crate reads what it published.
//!
//! One write of one crate:
//!
//! 1. a group whose change only inserted ops that bind no name (an impl, a builtin derive impl,
//!    an unnamed `const`, an `extern` block) has them put into its module's published scope,
//!    with no run;
//! 2. the other groups whose log changed or that are new are the region; a change that removed
//!    an op, a group gone and a binding of a dependency that changed bring in their readers,
//!    transitively (DRed's over-delete, since a glob cycle can hold a binding up by itself);
//! 3. the region runs from its round-0 logs over the rest of the crate as the state has it, to
//!    the fixed point `collect_block`'s loop reaches; its rows alone are taken out of the
//!    crate's table and put back;
//! 4. each key the run changed (a name of a module, or the module whole) wakes the groups
//!    outside the region that read it, and the grown region runs again from scratch, until a
//!    run changes no key anyone outside it read: the cutoff;
//! 5. every scope that changed is set; each group of the region makes its share of the name
//!    records again, and the crate's resolution is set in the parts that changed.
//!
//! A change of the crate env (an `extern crate`, a `#[macro_use]`, the std prelude), which no
//! read notes, runs every group. A crate graph change, a crate first seen and a state lost to
//! a panic run the crate from empty: the cold run, which is the same computation over every
//! group.

mod assemble;
mod diff;
mod input;
mod publish;
pub(in crate::nameres) mod reads;
mod run;
mod state;
#[cfg(test)]
mod tests;

use base_db::{Crate, Db, Written};
use blaze_dbsp::{Pos, Weight, Work};
use blaze_mem::{Frame, Tag};
use hir_expand::name::Name;
use rustc_hash::{FxHashMap, FxHashSet};

pub(in crate::nameres) use self::publish::{
    input_children, input_legacy_macro, input_name, input_scope, input_traits, published, published_module,
    published_modules, published_records,
};
pub(in crate::nameres) use self::state::State;
#[cfg(test)]
pub(in crate::nameres) use self::publish::Slices;
use self::{
    diff::{Changed, Items},
    input::Delta,
    run::RunOut,
    state::{CrateState, Group, Key, Scratch, over_delete},
};
use crate::{
    BuiltinDeriveImplId, ImplId, ModuleDefId, ModuleId, TraitId,
    item_scope::ItemScope,
    nameres::{
        BuildingScope, ModuleData, ModulesMap, ScopeSlot,
        facts::{ops::DeclOp, queries},
        frozen,
    },
};

/// What the maintainer reports its work under ([`Db::report_work`]).
const NAME: &str = "nameres";

inventory::submit! {
    base_db::Maintainer { order: 100, name: NAME, run: maintain }
}

/// What the last write changed of each maintained crate's resolution, in dependency order:
/// what the impls maintainer (`200`) reads. Cleared at the start of each write.
#[derive(Default)]
pub struct LastWrite {
    pub crates: Vec<CrateDelta>,
}

/// One crate's share of [`LastWrite`]; a crate the write left alone has none.
pub struct CrateDelta {
    pub krate: Crate,
    /// The crate was maintained from empty: every impl, derive impl and trait it has is an
    /// insert, and nothing held of it before stands.
    pub fresh: bool,
    /// Impls gone (`-1`, at the position they were handed over at) or new or moved (`+1`).
    pub impls: Vec<(ImplId, Pos, Weight)>,
    pub derive_impls: Vec<(BuiltinDeriveImplId, Pos, Weight)>,
    /// Traits declared (`+1`) or no longer (`-1`).
    pub traits: Vec<(TraitId, Weight)>,
    /// The names of modules whose meaning changed.
    pub bindings: Vec<(ModuleId, Name)>,
    /// The items and impls of the modules whose source was written or whose log changed: their
    /// headers and signatures may differ with no binding changed.
    pub touched: Vec<ModuleDefId>,
    pub touched_impls: Vec<ImplId>,
}

/// The maintainer: every maintained crate, in dependency order, by what `written` changed.
pub(crate) fn maintain(db: &mut Db, written: &Written<'_>) {
    let mut last = db.with_state::<LastWrite, _>(|_, it| std::mem::take(&mut it.crates));
    last.clear();
    db.with_state::<State, _>(|db, state| maintain_all(db, written, state, &mut last));
    db.with_state::<LastWrite, _>(|_, it| it.crates = last);
}

fn maintain_all(db: &mut Db, written: &Written<'_>, state: &mut State, last: &mut Vec<CrateDelta>) {
    let crates = base_db::all_crates(db);
    if written.crates {
        let live: FxHashSet<Crate> = crates.iter().copied().collect(); // alloc: a crate graph change, once
        let gone: Vec<Crate> = state.crates.keys().copied().filter(|it| !live.contains(it)).collect(); // alloc: as above
        for krate in gone {
            state.crates.remove(&krate);
            publish::enter(db, krate, None);
        }
    }
    // The modules whose row, children or textual macros changed, of the crates maintained so
    // far: `LastWrite` carries names only.
    let mut modules_changed: Vec<ModuleId> = Vec::new(); // alloc: grows with what the write changed
    for &krate in crates.iter() {
        if frozen::is_served(db, krate) {
            if state.crates.remove(&krate).is_some() {
                publish::enter(db, krate, None);
            }
            continue;
        }
        let mut work = Work::default();
        carry_maintaining();
        let _maintaining = Maintaining { before: replace_maintaining(Some(krate)) };
        if let Some(delta) = maintain_crate(db, krate, state, written, last, &mut modules_changed, &mut work) {
            last.push(delta);
        }
        db.report_work(NAME, krate, work);
    }
}

thread_local! {
    static MAINTAINING: std::cell::Cell<Option<Crate>> = const { std::cell::Cell::new(None) };
}

/// Carries [`MAINTAINING`] with each job on a fiber across its waits ([`blaze_query::carry`]):
/// a maintainer run that waits on a query resumes maintaining its crate, wherever it resumes.
fn carry_maintaining() {
    static CARRIED: std::sync::Once = std::sync::Once::new();
    CARRIED.call_once(|| {
        blaze_query::carry(blaze_query::Carried {
            take: || stdx::lend::take_carried_cell(&MAINTAINING, Option::is_none),
            put: |it| stdx::lend::put_carried_cell(&MAINTAINING, Option::is_none, it),
        });
    });
}

/// Restores the crate maintained before when it drops.
struct Maintaining {
    before: Option<Crate>,
}

impl Drop for Maintaining {
    fn drop(&mut self) {
        replace_maintaining(self.before);
    }
}

// Each access to `MAINTAINING` is a call of its own, never inlined: a maintainer run that waits
// between two may resume on another thread, and an inlined access may reuse the address of the
// first thread's slot (docs/v2/plan/lanes/may-suspend.md).

#[inline(never)]
fn replace_maintaining(krate: Option<Crate>) -> Option<Crate> {
    MAINTAINING.replace(krate)
}

/// The crate the maintainer is resolving on this thread, if any.
#[inline(never)]
pub(in crate::nameres) fn maintaining() -> Option<Crate> {
    MAINTAINING.get()
}

fn maintain_crate(
    db: &mut Db,
    krate: Crate,
    state: &mut State,
    written: &Written<'_>,
    earlier: &[CrateDelta],
    modules_changed: &mut Vec<ModuleId>,
    work: &mut Work,
) -> Option<CrateDelta> {
    let root = queries::crate_root(db, krate);
    let (cold, mut cs) = match state.crates.remove(&krate) {
        Some(it) if !written.crates && it.root == root => (false, it),
        old => (true, CrateState::cold(root, old)),
    };
    let deps_changed =
        !modules_changed.is_empty() || earlier.iter().any(|it| it.fresh || !it.bindings.is_empty());
    let mut delta = input::read(db, krate, &mut cs, written, cold, deps_changed);
    work.ops += delta.ops;
    // The root of a crate exporting procedural macros is published censored, and its state row
    // keeps the scope before the censor: a run makes both. A crate that starts or stops exporting
    // them (`#![crate_type]`) has its root censored or no longer, which only a run publishes.
    let proc_macro = queries::crate_data(db, krate).proc_macro;
    let flipped = cs.data_of.as_ref().is_some_and(|it| it.proc_macro != proc_macro);
    if let Some(at) = delta.direct.iter().position(|(it, _)| *it == cs.root)
        && (proc_macro || flipped)
    {
        let (group, _) = delta.direct.swap_remove(at);
        delta.seeds.push(group);
    } else if flipped && !delta.seeds.contains(&cs.root) {
        delta.seeds.push(cs.root);
    }
    let moved = match delta.tree {
        true => diff::update_positions(&mut cs),
        false => Vec::new(), // alloc: empty, allocates nothing
    };
    let mut wakes = dependency_wakes(db, &cs, earlier, modules_changed);
    wakes.extend(enum_glob_wakes(db, &cs, &delta, earlier));
    let mut region = initial_region(&cs, &mut state.scratch, &delta, wakes, cold);
    if region.is_empty() && !delta.deleted.is_empty() {
        // The rows and inputs of a group gone go with a run, which reports their keys.
        region.insert(cs.root);
    }

    let cutoff = match region.is_empty() {
        true => None,
        false => {
            let rows = std::mem::replace(&mut cs.rows, ModulesMap::new());
            let view: &Db = db;
            let scratch = &mut state.scratch;
            let deleted = &delta.deleted;
            let last = blaze_mem::frame::task(Tag::Nameres, |frame| {
                cutoff(view, frame, krate, &cs, scratch, deleted, &mut region, rows, work)
            });
            work.region += region.len() as u64;
            Some(last)
        }
    };
    // An edit of the root's `#![...]` alone wakes no group, and changes the crate data.
    let attrs_changed = cs.data_of.as_ref() != Some(queries::crate_data(db, krate));
    if cutoff.is_none() && delta.direct.is_empty() && moved.is_empty() && !attrs_changed {
        let out = touched_only(db, krate, &cs, &delta);
        state.crates.insert(krate, cs);
        return out;
    }
    let out = publish_crate(db, krate, &mut cs, &delta, cutoff, &region, &moved, cold, modules_changed, work);
    state.crates.insert(krate, cs);
    Some(out)
}

/// The groups of this crate that read a binding an earlier crate's write changed.
fn dependency_wakes(
    db: &Db,
    cs: &CrateState,
    earlier: &[CrateDelta],
    modules_changed: &[ModuleId],
) -> Vec<ModuleId> {
    let mut out = Vec::new(); // alloc: the groups woken across crates, once per write
    for delta in earlier {
        if delta.fresh {
            // Every module of that crate may be new: each group that read any is woken.
            for &group in cs.groups.keys() {
                if cs.module_readers.keys(group).any(|it| it.krate(db) == delta.krate) {
                    out.push(group);
                }
            }
            continue;
        }
        for &(module, name) in &delta.bindings {
            out.extend(cs.reads.readers((module, Some(name))));
            out.extend(cs.reads.readers((module, None)));
        }
    }
    for &module in modules_changed {
        out.extend(cs.reads.readers((module, None)));
    }
    out
}

/// The groups that glob-imported an enum the write may have changed: one declared by a group
/// of this crate it read again, or one an earlier crate's write touched. A variant is no
/// binding of the enum's module, so nothing else wakes them; the glob is noted reading
/// [`reads::enum_glob`] there, which no glob of the module reads. The same for the imports of a
/// trait's items ([`reads::trait_import`]).
fn enum_glob_wakes(db: &Db, cs: &CrateState, delta: &Delta, earlier: &[CrateDelta]) -> Vec<ModuleId> {
    let mut out = Vec::new(); // alloc: the groups woken, once per write that touches an enum
    let key = reads::enum_glob();
    let mut wake = |def: ModuleDefId| match def {
        ModuleDefId::AdtId(crate::AdtId::EnumId(it)) => {
            out.extend(cs.reads.readers((crate::Lookup::lookup(&it, db).container, Some(key))));
        }
        ModuleDefId::TraitId(it) => {
            out.extend(cs.reads.readers((crate::Lookup::lookup(&it, db).container, Some(reads::trait_import()))));
        }
        _ => {}
    };
    let direct = delta.direct.iter().map(|(it, _)| it);
    for group in delta.touched.iter().chain(&delta.seeds).chain(direct) {
        let Some(g) = cs.groups.get(group) else { continue };
        for module in &g.modules {
            let Some(row) = cs.rows.get(module) else { continue };
            for def in row.scope(db).declarations() {
                wake(def);
            }
        }
    }
    for it in earlier {
        for &def in &it.touched {
            wake(def);
        }
    }
    out
}

/// The groups the write runs first: every one when cold; else the changed and new ones, and
/// the readers of what a removal, a group gone or a dependency's change may have taken away,
/// transitively.
fn initial_region(
    cs: &CrateState,
    scratch: &mut Scratch,
    delta: &Delta,
    wakes: Vec<ModuleId>,
    cold: bool,
) -> FxHashSet<ModuleId> {
    let mut region = FxHashSet::default();
    if cold {
        region.extend(cs.groups.keys().copied());
        return region;
    }
    region.extend(delta.seeds.iter().copied());
    let mut redo = wakes;
    redo.extend(delta.removing.iter().copied());
    for (_, group) in &delta.deleted {
        for &module in &group.modules {
            redo.extend(cs.module_readers.readers(module));
        }
    }
    over_delete(cs, scratch, &redo, &mut region);
    if delta.root_ops {
        region.insert(cs.root);
    }
    region.retain(|it| cs.groups.contains_key(it));
    region
}

/// The root runs with every group whose expansions write its scope, and those with it.
fn close_root(cs: &CrateState, region: &mut FxHashSet<ModuleId>) {
    let writers = || cs.groups.iter().filter(|(_, it)| it.out.writes_root).map(|(&it, _)| it);
    if !region.contains(&cs.root) && !writers().any(|it| region.contains(&it)) {
        return;
    }
    region.insert(cs.root);
    region.extend(writers());
}

/// What the last run of the cutoff loop made, with the rows it replaced.
struct Cutoff {
    out: RunOut,
    /// The rows before the write of the region's modules and of the groups gone.
    old: FxHashMap<ModuleId, ModuleData>,
    /// Every other row.
    outside: ModulesMap,
    changed: Vec<Changed>,
    /// The region, in group order.
    order: Vec<ModuleId>,
}

#[allow(clippy::too_many_arguments)]
fn cutoff(
    db: &Db,
    frame: &Frame,
    krate: Crate,
    cs: &CrateState,
    scratch: &mut Scratch,
    deleted: &[(ModuleId, Group)],
    region: &mut FxHashSet<ModuleId>,
    mut rows: ModulesMap,
    work: &mut Work,
) -> Cutoff {
    let root = cs.root;
    loop {
        close_root(cs, region);
        let order = region_order(cs, region);
        let (outside, old) = split(cs, rows, &order, deleted, work);
        let (out, outside) = run::run(db, frame, krate, cs, &order, outside);
        work.rebuilt += out.rows.len() as u64;

        let every = order.len() == cs.groups.len();
        let widen = if out.missed_root && !region.contains(&root) {
            region.insert(root);
            true
        } else if !every && (out.env != cs.env || out.prelude != cs.prelude) {
            // The crate env is read by every path and noted by none.
            region.extend(cs.groups.keys().copied());
            true
        } else {
            false
        };
        if widen {
            rows = join(outside, old, work);
            continue;
        }

        let mut changed = Vec::new(); // alloc: the keys the run changed, once per run
        for (&module, row) in &old {
            diff::keys(db, module, Some(row), out.rows.get(&module), &mut changed);
        }
        for (module, row) in out.rows.iter() {
            if !old.contains_key(&module) {
                diff::keys(db, module, None, Some(row), &mut changed);
            }
        }
        // Each reader once: a group noted under a name and under the module whole, or woken by
        // an addition and a removal, counts once, as a removal's reader.
        let (mut woken, mut redo) = (FxHashSet::default(), FxHashSet::default()); // alloc: the readers outside the region, once per run
        for &((module, name), present) in &changed {
            let into = if present { &mut redo } else { &mut woken };
            readers_outside(cs, region, (module, name), into);
            if name.is_some() {
                readers_outside(cs, region, (module, None), into);
            }
        }
        woken.retain(|it| !redo.contains(it));
        if woken.is_empty() && redo.is_empty() {
            return Cutoff { out, old, outside, changed, order };
        }
        work.woken += (woken.len() + redo.len()) as u64;
        region.extend(woken);
        let redo: Vec<ModuleId> = redo.into_iter().collect(); // alloc: as above
        over_delete(cs, scratch, &redo, region);
        region.retain(|it| cs.groups.contains_key(it));
        rows = join(outside, old, work);
    }
}

/// The region in group order.
fn region_order(cs: &CrateState, region: &FxHashSet<ModuleId>) -> Vec<ModuleId> {
    let mut order: Vec<ModuleId> = region.iter().copied().collect(); // alloc: the region, once per run
    order.sort_unstable_by_key(|it| cs.groups.get_index_of(it));
    order
}

fn readers_outside(cs: &CrateState, region: &FxHashSet<ModuleId>, key: Key, into: &mut FxHashSet<ModuleId>) {
    into.extend(cs.reads.readers(key).filter(|it| !region.contains(it) && cs.groups.contains_key(it)));
}

/// The rows of the region's modules and of the groups gone taken out of `rows`, which is then
/// what a run of the region reads: each a swap with the table's last row, so a run moves rows
/// as many as its region has, whatever the crate's size.
fn split(
    cs: &CrateState,
    mut rows: ModulesMap,
    order: &[ModuleId],
    deleted: &[(ModuleId, Group)],
    work: &mut Work,
) -> (ModulesMap, FxHashMap<ModuleId, ModuleData>) {
    let mut old = FxHashMap::default();
    let ran = order.iter().flat_map(|group| &cs.groups[group].modules);
    for &module in ran.chain(deleted.iter().flat_map(|(_, group)| &group.modules)) {
        if let Some(row) = rows.swap_remove(&module) {
            work.moved += 1;
            old.insert(module, row);
        }
    }
    (rows, old)
}

fn join(mut outside: ModulesMap, old: FxHashMap<ModuleId, ModuleData>, work: &mut Work) -> ModulesMap {
    work.moved += old.len() as u64;
    outside.extend(old);
    outside
}

/// The state after the write, its scopes and resolution set where they changed, and the
/// crate's [`CrateDelta`].
#[allow(clippy::too_many_arguments)]
fn publish_crate(
    db: &mut Db,
    krate: Crate,
    cs: &mut CrateState,
    delta: &Delta,
    cutoff: Option<Cutoff>,
    region: &FxHashSet<ModuleId>,
    moved: &[ModuleId],
    cold: bool,
    modules_changed: &mut Vec<ModuleId>,
    work: &mut Work,
) -> CrateDelta {
    // clone: a count of the crate's attributes, held across the writes to `db` below
    let attrs = queries::crate_data(db, krate).clone();
    let proc_macro = attrs.proc_macro;
    let root = cs.root;
    // What the impls maintainer is handed, before: the items of every module the write replaced.
    let mut old_items = Items::default();
    let mut changed = Vec::new(); // alloc: empty, allocates nothing without a run
    let mut order = Vec::new(); // alloc: as above
    // Which parts of the crate's resolution the write may have changed.
    let mut rows_changed = cold || delta.tree;
    // A cold run keeps the old state's inputs, not what they were made from.
    let mut records_changed = cold || !delta.deleted.is_empty();
    let mut data_changed = cold
        || cs.data.is_none()
        || cs.data_of.as_ref() != Some(&attrs)
        || delta.deleted.iter().any(|(_, it)| !it.out.crate_ops.is_empty());
    let mut env_changed = false;

    if let Some(Cutoff { out: mut run, old, mut outside, changed: keys, order: ran }) = cutoff {
        for row in old.values() {
            old_items.add(row.scope(db), &Pos::root());
        }

        // Each scope the run made, set where it changed, and its row put back.
        let mut new_rows = std::mem::replace(&mut run.rows, ModulesMap::new());
        for (module, row) in new_rows.iter_mut() {
            let ScopeSlot::Building(building) = &mut row.scope else {
                unreachable!("nameres delta: a run's row is `ScopeSlot::Building`")
            };
            // A crate exporting procedural macros exports nothing else: its root is published
            // censored, and its own paths read the scope before the censor, which the state keeps.
            let censored = module == root && proc_macro;
            let BuildingScope { mut scope, children } = match censored {
                // clone: the proc-macro root's scope, of which the state keeps the uncensored one
                true => BuildingScope::clone(building),
                false => std::mem::take(&mut **building),
            };
            if censored {
                scope.censor_non_proc_macros(krate);
            }
            let mut slot = cs.inputs.get(&module).copied();
            work.slices += u64::from(publish::set_scope(db, &mut slot, module, scope, children));
            let input = slot.expect("nameres delta: `set_scope` makes the input");
            cs.inputs.insert(module, input);
            if !censored {
                row.scope = ScopeSlot::Input(publish::scope_id(input));
            }
        }
        for (module, row) in new_rows.drain(..) {
            rows_changed |= old.get(&module) != Some(&row);
            outside.insert(module, row);
            work.moved += 1;
        }
        rows_changed |= old.keys().any(|it| !outside.contains_key(it));
        cs.rows = outside;

        // The groups' modules and outputs.
        for (group, gone) in &delta.deleted {
            for module in gone.modules.iter().filter(|it| !run.group_of.contains_key(it)) {
                cs.inputs.remove(module);
                cs.group_of.remove(module);
            }
            cs.reads.forget(*group);
            cs.module_readers.forget(*group);
        }
        for &group in &ran {
            let (out, modules) = run.outs.remove(&group).unwrap_or_default();
            let g = cs.groups.get_mut(&group).expect("nameres delta: the region is of the crate's groups");
            for module in &g.modules {
                if !run.group_of.contains_key(module) {
                    cs.inputs.remove(module);
                    cs.group_of.remove(module);
                }
            }
            records_changed |= g.out != out;
            data_changed |= g.out.crate_ops != out.crate_ops;
            rows_changed |= g.modules != modules;
            g.out = out;
            g.modules = modules;
        }
        cs.group_of.extend(run.group_of.iter().map(|(&module, &group)| (module, group)));

        env_changed = run.env != cs.env || run.prelude != cs.prelude;
        cs.env = run.env;
        cs.prelude = run.prelude;
        cs.prelude_holder = run.prelude_holder;
        if data_changed {
            cs.data = Some(assemble::data(db, krate, cs));
            cs.data_of = Some(attrs.clone()); // clone: the run keeps its own for the rows after
        }

        // Each group of the region's share of the records, on the crate as it now is: what it
        // reads is noted as its own, so a key that changes later wakes it like its run's reads.
        let local = cs.env.local_def_map();
        let rows = std::mem::replace(&mut cs.rows, ModulesMap::new());
        let view = assemble::view(krate, cs, rows);
        let shares = assemble::shares(db, cs, &view, &local, &ran, &mut run.reads);
        cs.rows = assemble::rows_of(view);
        for (&group, share) in ran.iter().zip(shares) {
            let g = cs.groups.get_mut(&group).expect("nameres delta: the region is of the crate's groups");
            records_changed |= g.records != share;
            g.records = share;
        }

        // Who read what, the region's reads replacing its old ones. A group's reads of its own
        // modules are not kept: it runs whenever they change.
        for &group in &ran {
            cs.reads.forget(group);
            cs.module_readers.forget(group);
        }
        for &(reader, (module, name)) in &run.reads {
            let owner = cs.group_of.get(&module).copied();
            let mut note = |group: ModuleId| {
                if owner != Some(group) {
                    cs.reads.add((module, name), group);
                    cs.module_readers.add(module, group);
                }
            };
            match reader.and_then(|it| cs.group_of.get(&it).copied()) {
                Some(group) => note(group),
                None => ran.iter().for_each(|&it| note(it)),
            }
        }
        changed = keys;
        order = ran;
    } else if data_changed {
        cs.data = Some(assemble::data(db, krate, cs));
        cs.data_of = Some(attrs.clone()); // clone: a count of the crate's attributes
    }

    // The name-free inserts of the groups that did not run.
    let mut emit: Vec<ModuleId> = order.clone(); // clone: the region's groups, once per write
    for (group, places) in &delta.direct {
        if !region.contains(group) {
            apply_direct(db, cs, *group, places, &mut old_items, work);
            emit.push(*group);
        }
    }
    emit.extend(moved.iter().copied().filter(|it| !region.contains(it)));
    emit.sort_unstable_by_key(|it| cs.groups.get_index_of(it));
    emit.dedup();

    if cs.resolution.is_none() || rows_changed || records_changed || data_changed || env_changed {
        let root_input = proc_macro.then(|| publish::scope_id(cs.inputs[&root]));
        let (data, session_macros) = cs.data.as_ref().expect("nameres delta: the crate data is made with a run");
        let assembled = publish::Assembled {
            root,
            modules: (rows_changed || cs.resolution.is_none()).then(|| assemble::modules(cs, root_input)),
            local: cs.env.local_def_map(),
            // clone: one count of the crate's shared data
            data: data.clone(),
            prelude: cs.prelude,
            macro_use_prelude: assemble::macro_use_prelude(krate, cs),
            session_macros: *session_macros,
            records: (records_changed || cs.resolution.is_none()).then(|| assemble::records(db, cs)),
        };
        let mut slot = cs.resolution;
        work.slices += publish::set_resolution(db, krate, &mut slot, assembled);
        cs.resolution = slot;
    }
    work.keys += changed.len() as u64;

    let mut crate_delta = touched_only(db, krate, cs, delta).unwrap_or_else(|| empty_delta(krate));
    crate_delta.fresh = cold;
    let mut new_items = Items::default();
    for group in &emit {
        let pos = cs.group_pos.get(group).expect("nameres delta: every group has a position");
        diff::items_of(db, &cs.rows, &cs.groups[group].modules, pos, &mut new_items);
    }
    let CrateDelta { impls, derive_impls, traits, .. } = &mut crate_delta;
    diff::items(cs, &old_items, new_items, impls, derive_impls, traits);
    for &((module, name), _) in &changed {
        match name {
            Some(name) => crate_delta.bindings.push((module, name)),
            None => modules_changed.push(module),
        }
    }
    crate_delta
}

/// Puts a group's name-free inserts (`places` in its new log) into its module's published
/// scope, where a run would put them: a round-0 op of a list is replayed before any expansion
/// adds to it, so its index is the number of the same ops before it in the log.
fn apply_direct(db: &mut Db, cs: &CrateState, group: ModuleId, places: &[u32], old_items: &mut Items, work: &mut Work) {
    let input = *cs.inputs.get(&group).unwrap_or_else(|| panic!("nameres delta: no scope input of {group:?}"));
    // clone: the module's scope, which the inserts change, once per module a write inserts into
    let mut scope = ItemScope::clone(input.scope(db));
    old_items.add(&scope, &Pos::root());
    let log = &cs.groups[&group].log;
    for &at in places {
        let at = at as usize;
        let before = &log[..at];
        let index = |same: fn(&DeclOp) -> bool| before.iter().filter(|op| same(op) && op.target() == Some(group)).count();
        match log[at] {
            DeclOp::DefineImpl { module, imp, is_trait_impl, is_reservation } if module == group => {
                let at = index(|op| matches!(op, DeclOp::DefineImpl { .. }));
                insert_impl(&mut scope, at, imp, is_trait_impl, is_reservation);
            }
            DeclOp::DefineBuiltinDeriveImpl { module, imp } if module == group => {
                let at = index(|op| matches!(op, DeclOp::DefineBuiltinDeriveImpl { .. }));
                scope.builtin_derive_impls.insert(at, imp);
            }
            DeclOp::DefineUnnamedConst { module, id } if module == group => {
                let at = index(|op| matches!(op, DeclOp::DefineUnnamedConst { .. }));
                scope.unnamed_consts.insert(at, id);
            }
            DeclOp::DefineExternBlock { module, id } if module == group => {
                let at = index(|op| matches!(op, DeclOp::DefineExternBlock { .. }));
                scope.extern_blocks.insert(at, id);
            }
            // An op of another module, which the group's run does not replay either.
            _ => {}
        }
    }
    work.slices += u64::from(publish::set_scope_alone(db, input, scope));
}

/// `scope` with `imp` at `at` of its impls, its bits as `define_impl` in order makes them.
fn insert_impl(scope: &mut ItemScope, at: usize, imp: ImplId, is_trait_impl: bool, is_reservation: bool) {
    let tail: Vec<(ImplId, bool, bool)> = (at..scope.impls.len())
        .map(|it| (scope.impls[it], scope.trait_impl.contains(it), scope.reservation_impl.contains(it)))
        .collect(); // alloc: the impls after `at`, once per impl inserted
    scope.impls.truncate(at);
    scope.define_impl(imp, is_trait_impl, is_reservation);
    for (imp, is_trait_impl, is_reservation) in tail {
        scope.define_impl(imp, is_trait_impl, is_reservation);
    }
}

/// The crate's def map as a maintainer from empty makes it, outside the database's state and
/// its published inputs: the cold run, which the tests compare a maintained map with. Its
/// scopes are being built (`ModulesSlot::Building`), its records in the map.
#[cfg(test)]
fn cold(db: &Db, frame: &Frame, krate: Crate) -> (crate::nameres::DefMap, crate::nameres::LocalDefMap) {
    use crate::nameres::{DefMap, DiagnosticsByModule, ModulesSlot, OwnedRecords};

    let root = queries::crate_root(db, krate);
    let mut cs = CrateState::new(root);
    let none = Written { set: &[], added: &[], removed: &[], roots: &[], crates: true, included: &[] };
    input::read(db, krate, &mut cs, &none, true, false);
    let mut region: FxHashSet<ModuleId> = cs.groups.keys().copied().collect();
    let mut work = Work::default();
    let Cutoff { mut out, mut outside, order, .. } =
        cutoff(db, frame, krate, &cs, &mut Scratch::default(), &[], &mut region, ModulesMap::new(), &mut work);
    for &group in &order {
        let (share, modules) = out.outs.remove(&group).unwrap_or_default();
        let g = cs.groups.get_mut(&group).expect("nameres delta: the region is of the crate's groups");
        g.out = share;
        g.modules = modules;
    }
    cs.group_of = std::mem::take(&mut out.group_of);
    outside.extend(out.rows.drain(..));
    cs.rows = outside;
    cs.env = out.env;
    cs.prelude = out.prelude;
    cs.data = Some(assemble::data(db, krate, &cs));
    cs.data_of = Some(queries::crate_data(db, krate).clone()); // clone: a count of the crate's attributes

    let local = cs.env.local_def_map();
    let rows = std::mem::replace(&mut cs.rows, ModulesMap::new());
    let view = assemble::view(krate, &cs, rows);
    let shares = assemble::shares(db, &cs, &view, &local, &order, &mut Vec::new());
    cs.rows = assemble::rows_of(view);
    for (&group, share) in order.iter().zip(shares) {
        cs.groups.get_mut(&group).expect("nameres delta: the region is of the crate's groups").records = share;
    }
    let OwnedRecords { names, diagnostics, derive_helpers_in_scope, .. } = assemble::records(db, &cs);
    let mut modules = cs.ordered_rows();
    if queries::crate_data(db, krate).proc_macro {
        modules[root].scope_mut().censor_non_proc_macros(krate);
    }
    let macro_use_prelude = assemble::macro_use_prelude(krate, &cs);
    let (data, session_macros) = cs.data.take().expect("nameres delta: made above");
    let def_map = DefMap {
        krate,
        block: None,
        root,
        modules: ModulesSlot::Building(modules),
        prelude: cs.prelude,
        macro_use_prelude,
        derive_helpers_in_scope,
        session_macros,
        names,
        diagnostics,
        diagnostics_by_module: DiagnosticsByModule::default(),
        records: None,
        data,
    };
    (def_map, local)
}

fn empty_delta(krate: Crate) -> CrateDelta {
    CrateDelta {
        krate,
        fresh: false,
        impls: Vec::new(),         // alloc: empty, allocates nothing
        derive_impls: Vec::new(),  // alloc: as above
        traits: Vec::new(),        // alloc: as above
        bindings: Vec::new(),      // alloc: as above
        touched: Vec::new(),       // alloc: as above
        touched_impls: Vec::new(), // alloc: as above
    }
}

/// The items and impls of the groups the write read again (a file of theirs written, or their
/// log changed), as the state has them: `None` when there are none.
fn touched_only(db: &Db, krate: Crate, cs: &CrateState, delta: &Delta) -> Option<CrateDelta> {
    let direct = delta.direct.iter().map(|(it, _)| it);
    let mut groups: Vec<ModuleId> = delta.touched.iter().chain(&delta.seeds).chain(direct).copied().collect(); // alloc: the groups a write read again
    groups.sort_by_key(|it| cs.groups.get_index_of(it));
    groups.dedup();
    let mut out = empty_delta(krate);
    for group in groups {
        let Some(g) = cs.groups.get(&group) else { continue };
        for module in &g.modules {
            let Some(row) = cs.rows.get(module) else { continue };
            let scope = row.scope(db);
            out.touched.extend(scope.declarations());
            out.touched_impls.extend(scope.impls());
        }
    }
    (!out.touched.is_empty() || !out.touched_impls.is_empty()).then_some(out)
}
