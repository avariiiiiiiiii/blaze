//! The maintainer's state and one write's run (design §7, §8): the impls held per crate with
//! their keys, the arrangements the slices are published from, and the read sets that say
//! which keys a write may have changed.
//!
//! A run goes over the crates in dependency order, skipping frozen ones. For each it applies
//! name resolution's delta (impls and derives added or removed, traits declared or gone),
//! recomputes the keys of the impls the write woke and the body parts of the modules it may
//! have changed the bodies of, publishes the slices whose rows changed, and hands the next
//! maintainer what changed ([`ImplsDelta`]). A key or body part is recomputed through the pure
//! queries, so recomputing one that did not change costs memo validations and changes nothing.

use base_db::{Crate, Db, Written};
use blaze_dbsp::{Arrangement, Pos, ReadIndex, Work};
use blaze_mem::Tag;
use hir_def::{
    BuiltinDeriveImplId, HasModule, ImplId, ModuleDefId, ModuleId, TraitId,
    builtin_derive::BuiltinDeriveImplTrait,
    lang_item::lang_items,
    nameres::{
        delta::{CrateDelta, LastWrite},
        crate_def_map, module_impls,
    },
    signatures::ImplSignature,
};
use hir_expand::name::Name;
use intern::Symbol;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use super::{
    ImplsDelta,
    head::Head,
    key::{ImplKey, Reads, assoc_names, derive_key, derive_module, impl_key, impl_reads},
    publish::{CandidateRows, InherentRows, Published, TraitRows},
    read::{BodyPart, body_part},
};
use crate::method_resolution::{frozen, macros_writing_impls};

/// What the impls maintainer reports its work under ([`Db::report_work`]).
pub(crate) const NAME: &str = "impls";

/// A row of a trait's arrangement: an impl with its head, or a built-in derive impl.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TraitEntry {
    Impl(ModuleId, ImplId, Option<Head>),
    Derive(ModuleId, BuiltinDeriveImplId, Head),
}

struct ImplRow {
    module: ModuleId,
    pos: Pos,
    key: ImplKey,
    trait_impl: bool,
}

struct DeriveRow {
    module: ModuleId,
    pos: Pos,
    key: Option<(TraitId, Head)>,
}

/// One crate's impls and traits, and its published inputs.
#[derive(Default)]
pub(crate) struct CrateState {
    impls: FxHashMap<ImplId, ImplRow>,
    derives: FxHashMap<BuiltinDeriveImplId, DeriveRow>,
    pub(crate) traits: Arrangement<TraitId, Pos, TraitEntry>,
    pub(crate) heads: Arrangement<Head, Pos, (ModuleId, ImplId)>,
    /// Each trait declared in the crate's modules with its item names (`AssocFn`).
    assoc: FxHashMap<TraitId, SmallVec<[Name; 8]>>,
    /// Traits whose items a macro call writes: their names may change with no edit of theirs.
    wild_traits: FxHashSet<TraitId>,
    pub(crate) candidates: Arrangement<Name, TraitId, TraitId>,
    /// The traits the crate's lang items give each built-in derive, by
    /// `BuiltinDeriveImplTrait::ALL` index, once a derive impl asked.
    lang: Option<SmallVec<[Option<TraitId>; 16]>>,
    /// The body part of each module whose bodies may hold an impl.
    bodies: FxHashMap<ModuleId, BodyPart>,
    /// The modules whose bodies hold impls of each trait, of each head, and (under `()`) any
    /// inherent impl.
    body_traits: Arrangement<TraitId, ModuleId, ModuleId>,
    body_heads: Arrangement<Head, ModuleId, ModuleId>,
    body_inherent: Arrangement<(), ModuleId, ModuleId>,
    /// What `macros_writing_impls` gave the last write: which macro calls may write an impl
    /// in a body, so a change of it reads every module's bodies again.
    macros: Option<Box<[(Symbol, bool)]>>,
    published: Published,
}

/// Every crate's state, and the read sets across crates: an alias edited in a dependency wakes
/// the impls of its dependents that read it.
#[derive(Default)]
pub struct State {
    crates: FxHashMap<Crate, CrateState>,
    owner: FxHashMap<ImplId, Crate>,
    by_name: ReadIndex<Name, ImplId>,
    by_def: ReadIndex<ModuleDefId, ImplId>,
    wild: FxHashSet<ImplId>,
    scratch: Scratch,
}

/// Buffers kept from one write to the next.
#[derive(Default)]
struct Scratch {
    reads: Reads,
    deltas: FxHashMap<Crate, usize>,
    names: FxHashSet<Name>,
    defs: FxHashSet<ModuleDefId>,
    woken: FxHashSet<ImplId>,
    done: FxHashSet<ImplId>,
    kinds: FxHashMap<ImplId, bool>,
    kinds_of: FxHashSet<ModuleId>,
    trait_keys: Vec<TraitId>,
    head_keys: Vec<Head>,
    name_keys: Vec<Name>,
    redo_traits: Vec<TraitId>,
    assoc: SmallVec<[Name; 8]>,
    body_modules: Vec<ModuleId>,
    /// The impls a delta adds, each with whether it is a trait impl.
    added: Vec<(ImplId, bool)>,
    /// The modules of the crates a write sweeps, every crate's in graph order.
    swept: Vec<ModuleId>,
    units: Vec<()>,
    keys_changed: Vec<ImplId>,
    traits_changed: Vec<TraitId>,
}

impl State {
    /// Whether `krate` is maintained here.
    #[cfg(test)]
    pub(crate) fn crate_state(&self, krate: Crate) -> Option<&CrateState> {
        self.crates.get(&krate)
    }
}

/// One write's run over every crate; what changed of each, into `out`.
pub(crate) fn run(
    db: &mut Db,
    state: &mut State,
    last: &LastWrite,
    written: &Written<'_>,
    out: &mut Vec<ImplsDelta>,
) {
    let crates = base_db::all_crates(&*db);
    if written.crates {
        drop_gone(state, &crates);
    }
    let scratch = &mut state.scratch;
    scratch.deltas.clear();
    scratch.names.clear();
    scratch.defs.clear();
    scratch.woken.clear();
    scratch.done.clear();
    scratch.kinds.clear();
    scratch.kinds_of.clear();
    for (at, delta) in last.crates.iter().enumerate() {
        scratch.deltas.insert(delta.krate, at);
        scratch.names.extend(delta.bindings.iter().map(|&(_, name)| name));
        scratch.defs.extend(delta.touched.iter().copied());
        scratch.woken.extend(delta.touched_impls.iter().copied());
    }
    for name in &scratch.names {
        scratch.woken.extend(state.by_name.readers(*name));
    }
    for def in &scratch.defs {
        scratch.woken.extend(state.by_def.readers(*def));
    }
    let changed = !scratch.names.is_empty() || !scratch.defs.is_empty();
    if changed {
        scratch.woken.extend(state.wild.iter().copied());
    }
    // A load (every crate fresh) or a crate graph change sweeps every module of the crates it
    // runs: their body parts are read as one batch across the crates on the database's workers,
    // so the crates' sweeps below diff memos.
    let mut swept = std::mem::take(&mut scratch.swept);
    swept.clear();
    for &krate in crates.iter() {
        let fresh = scratch.deltas.get(&krate).is_some_and(|&at| last.crates[at].fresh);
        if (fresh || written.crates) && !frozen::served(&*db, krate) {
            swept.extend(crate_def_map(&*db, krate).modules(&*db).map(|(module, _)| module));
        }
    }
    if swept.len() > 1 {
        db.run_all(Tag::Infer, swept.len(), |db, at| _ = body_part(db, swept[at]));
    }
    state.scratch.swept = swept;
    for &krate in crates.iter() {
        if frozen::served(&*db, krate) {
            continue;
        }
        let delta = state.scratch.deltas.get(&krate).map(|&at| &last.crates[at]);
        let work = run_crate(db, state, krate, delta, written.crates, changed, out);
        db.report_work(NAME, krate, work);
    }
}

/// Drops the state of every crate no longer in the graph, with its impls' read sets.
fn drop_gone(state: &mut State, crates: &[Crate]) {
    // alloc: a crate graph change, once
    let live: FxHashSet<Crate> = crates.iter().copied().collect();
    let gone: Vec<Crate> =
        state.crates.keys().copied().filter(|it| !live.contains(it)).collect(); // alloc: as above
    for krate in gone {
        let Some(crate_state) = state.crates.remove(&krate) else { continue };
        for imp in crate_state.impls.keys() {
            forget_reads(state, *imp);
            state.owner.remove(imp);
        }
    }
}

fn forget_reads(state: &mut State, imp: ImplId) {
    state.by_name.forget(imp);
    state.by_def.forget(imp);
    state.wild.remove(&imp);
}

fn run_crate(
    db: &mut Db,
    state: &mut State,
    krate: Crate,
    delta: Option<&CrateDelta>,
    crates_changed: bool,
    changed: bool,
    out: &mut Vec<ImplsDelta>,
) -> Work {
    let mut work = Work::default();
    let fresh = delta.is_some_and(|it| it.fresh);
    state.scratch.keys_changed.clear();
    state.scratch.traits_changed.clear();
    if fresh {
        reset(state, krate);
    } else if !state.crates.contains_key(&krate) {
        panic!(
            "impls: crate {krate:?} has no maintained impl tables and name resolution handed it \
             no fresh delta"
        );
    }
    let mut crate_state = state.crates.remove(&krate).unwrap_or_default();
    if fresh {
        let (traits, heads, names) = crate_state.published.keys();
        state.scratch.trait_keys.extend(traits);
        state.scratch.head_keys.extend(heads);
        state.scratch.name_keys.extend(names);
    }
    if let Some(delta) = delta {
        apply_impls(db, state, &mut crate_state, krate, delta, &mut work);
        apply_derives(db, &mut crate_state, delta, &mut work);
        apply_traits(db, state, &mut crate_state, delta, &mut work);
    }
    rekey(db, state, &mut crate_state, krate, crates_changed, &mut work);
    if crates_changed || changed {
        rename_wild_traits(db, state, &mut crate_state, &mut work);
    }
    let lang_check = fresh || crates_changed || delta.is_some_and(touches_traits);
    relang(db, &mut crate_state, krate, lang_check, &mut work);
    let sweep = fresh || crates_changed;
    if sweep || changed || delta.is_some() {
        apply_bodies(&*db, state, &mut crate_state, krate, delta, sweep, &mut work);
    }
    publish(db, state, &mut crate_state, krate, fresh, &mut work);
    state.crates.insert(krate, crate_state);
    let scratch = &mut state.scratch;
    if !scratch.keys_changed.is_empty() || !scratch.traits_changed.is_empty() {
        scratch.keys_changed.sort_unstable();
        scratch.keys_changed.dedup();
        scratch.traits_changed.sort_unstable();
        scratch.traits_changed.dedup();
        out.push(ImplsDelta {
            krate,
            // alloc: the columns move into the delta; the next write's grow anew
            keys_changed: std::mem::take(&mut scratch.keys_changed),
            traits_changed: std::mem::take(&mut scratch.traits_changed),
        });
    }
    work
}

/// Forgets what `krate` held, keeping its published inputs: a crate maintained from empty sets
/// every slice it had anew, emptied where it has no rows now.
fn reset(state: &mut State, krate: Crate) {
    let Some(old) = state.crates.remove(&krate) else { return };
    for imp in old.impls.keys() {
        forget_reads(state, *imp);
        state.owner.remove(imp);
    }
    state.scratch.keys_changed.extend(old.impls.keys().copied());
    state.scratch.traits_changed.extend(old.assoc.keys().copied());
    state.crates.insert(krate, CrateState { published: old.published, ..CrateState::default() });
}

fn touches_traits(delta: &CrateDelta) -> bool {
    !delta.traits.is_empty() || delta.touched.iter().any(|it| matches!(it, ModuleDefId::TraitId(_)))
}

/// Whether `imp` is a trait impl, by its module's scope: the split the tables take.
fn is_trait_impl(db: &Db, scratch: &mut Scratch, module: ModuleId, imp: ImplId) -> bool {
    if scratch.kinds_of.insert(module) {
        let impls = module_impls(db, module);
        scratch.kinds.extend(impls.trait_impls.iter().map(|&it| (it, true)));
        scratch.kinds.extend(impls.inherent_impls.iter().map(|&it| (it, false)));
    }
    *scratch.kinds.get(&imp).unwrap_or_else(|| {
        panic!("impls: {imp:?} is not among the impls of its module {module:?}")
    })
}

fn apply_impls(
    db: &mut Db,
    state: &mut State,
    crate_state: &mut CrateState,
    krate: Crate,
    delta: &CrateDelta,
    work: &mut Work,
) {
    for (imp, pos, _) in delta.impls.iter().filter(|it| it.2 < 0) {
        work.ops += 1;
        if pos_of(crate_state, *imp).is_some_and(|it| it == pos)
            && remove_impl(state, crate_state, *imp)
        {
            state.scratch.keys_changed.push(*imp);
        }
    }
    // An added impl's key and read set lower its header: the queries they read are run as one
    // batch on the database's workers, and the loop below reads their memos in the delta's order.
    let mut added = std::mem::take(&mut state.scratch.added);
    added.clear();
    for (imp, _, _) in delta.impls.iter().filter(|it| it.2 > 0) {
        let module = imp.loc(&*db).container;
        added.push((*imp, is_trait_impl(&*db, &mut state.scratch, module, *imp)));
    }
    if added.len() > 1 {
        db.run_all(Tag::Infer, added.len(), |db, at| {
            let (imp, trait_impl) = added[at];
            _ = impl_key(db, imp, trait_impl);
            _ = ImplSignature::of(db, imp);
        });
    }
    state.scratch.added = added;
    for (imp, pos, _) in delta.impls.iter().filter(|it| it.2 > 0) {
        work.ops += 1;
        remove_impl(state, crate_state, *imp);
        state.scratch.keys_changed.push(*imp);
        let module = imp.loc(&*db).container;
        let trait_impl = is_trait_impl(&*db, &mut state.scratch, module, *imp);
        let key = impl_key(&*db, *imp, trait_impl);
        work.rebuilt += 1;
        work.keys += 1;
        // clone: a position, inline to a depth of six
        let row = ImplRow { module, pos: pos.clone(), key, trait_impl };
        arrange(crate_state, *imp, &row, 1);
        crate_state.impls.insert(*imp, row);
        state.owner.insert(*imp, krate);
        note_reads(db, state, *imp);
        state.scratch.done.insert(*imp);
    }
}

fn pos_of(crate_state: &CrateState, imp: ImplId) -> Option<&Pos> {
    crate_state.impls.get(&imp).map(|row| &row.pos)
}

/// Drops `imp`'s row and read set; whether it had one.
fn remove_impl(state: &mut State, crate_state: &mut CrateState, imp: ImplId) -> bool {
    let Some(row) = crate_state.impls.remove(&imp) else { return false };
    arrange(crate_state, imp, &row, -1);
    forget_reads(state, imp);
    state.owner.remove(&imp);
    true
}

/// Adds `row` to the arrangements its key is in, with `weight`.
fn arrange(crate_state: &mut CrateState, imp: ImplId, row: &ImplRow, weight: i32) {
    match row.key {
        ImplKey::Skip => {}
        ImplKey::Trait { trait_, head } => crate_state.traits.update(
            trait_,
            // clone: a position, inline to a depth of six
            row.pos.clone(),
            TraitEntry::Impl(row.module, imp, head),
            weight,
        ),
        ImplKey::Inherent(head) => {
            // clone: a position, inline to a depth of six
            crate_state.heads.update(head, row.pos.clone(), (row.module, imp), weight)
        }
    }
}

fn arrange_derive(
    crate_state: &mut CrateState,
    imp: BuiltinDeriveImplId,
    row: &DeriveRow,
    weight: i32,
) {
    if let Some((trait_, head)) = row.key {
        let entry = TraitEntry::Derive(row.module, imp, head);
        // clone: a position, inline to a depth of six
        crate_state.traits.update(trait_, row.pos.clone(), entry, weight);
    }
}

/// Records what `imp`'s key read in the read indexes.
fn note_reads(db: &Db, state: &mut State, imp: ImplId) {
    let reads = &mut state.scratch.reads;
    impl_reads(db, imp, reads);
    for &name in &reads.names {
        state.by_name.add(name, imp);
    }
    for &def in &reads.defs {
        state.by_def.add(def, imp);
    }
    if reads.wild {
        state.wild.insert(imp);
    }
}

/// Recomputes the key of every impl of `krate` the write woke (all of them when the crate
/// graph changed): a changed key moves its row.
fn rekey(
    db: &mut Db,
    state: &mut State,
    crate_state: &mut CrateState,
    krate: Crate,
    all: bool,
    work: &mut Work,
) {
    let woken: Vec<ImplId> = if all {
        // alloc: the impls to rekey, once per crate graph change
        crate_state.impls.keys().copied().collect()
    } else {
        let woken = state.scratch.woken.iter().copied();
        // alloc: the impls to rekey, once per write that woke any
        woken.filter(|imp| state.owner.get(imp) == Some(&krate)).collect()
    };
    for imp in woken {
        if !state.scratch.done.insert(imp) {
            continue;
        }
        let Some(row) = crate_state.impls.get(&imp) else { continue };
        work.woken += 1;
        work.rebuilt += 1;
        let trait_impl = is_trait_impl(&*db, &mut state.scratch, row.module, imp);
        let key = impl_key(&*db, imp, trait_impl);
        forget_reads(state, imp);
        note_reads(db, state, imp);
        let row = crate_state.impls.remove(&imp).expect("the row just read");
        if key == row.key && trait_impl == row.trait_impl {
            crate_state.impls.insert(imp, row);
            continue;
        }
        work.keys += 1;
        state.scratch.keys_changed.push(imp);
        arrange(crate_state, imp, &row, -1);
        let row = ImplRow { key, trait_impl, ..row };
        arrange(crate_state, imp, &row, 1);
        crate_state.impls.insert(imp, row);
    }
}

fn apply_derives(db: &mut Db, crate_state: &mut CrateState, delta: &CrateDelta, work: &mut Work) {
    for (imp, pos, _) in delta.derive_impls.iter().filter(|it| it.2 < 0) {
        work.ops += 1;
        if crate_state.derives.get(imp).is_some_and(|row| row.pos == *pos)
            && let Some(row) = crate_state.derives.remove(imp)
        {
            arrange_derive(crate_state, *imp, &row, -1);
        }
    }
    for (imp, pos, _) in delta.derive_impls.iter().filter(|it| it.2 > 0) {
        work.ops += 1;
        if let Some(row) = crate_state.derives.remove(imp) {
            arrange_derive(crate_state, *imp, &row, -1);
        }
        let module = derive_module(&*db, *imp);
        let krate = module.krate(&*db);
        let key = derive_key(&*db, *imp, lang_items(&*db, krate));
        work.rebuilt += 1;
        work.keys += 1;
        // clone: a position, inline to a depth of six
        let row = DeriveRow { module, pos: pos.clone(), key };
        arrange_derive(crate_state, *imp, &row, 1);
        crate_state.derives.insert(*imp, row);
    }
}

/// Re-reads the traits the lang items give the built-in derives when a write may have changed
/// them (a trait declared, gone or re-read, or the crate graph); a changed one rekeys the
/// derive impls of that derive.
fn relang(db: &mut Db, crate_state: &mut CrateState, krate: Crate, check: bool, work: &mut Work) {
    if crate_state.derives.is_empty() || !(check || crate_state.lang.is_none()) {
        return;
    }
    let items = lang_items(&*db, krate);
    let now: SmallVec<[Option<TraitId>; 16]> =
        BuiltinDeriveImplTrait::ALL.iter().map(|it| it.get_id(items)).collect(); // alloc: inline
    if crate_state.lang.as_ref() == Some(&now) {
        return;
    }
    crate_state.lang = Some(now);
    // alloc: the derive impls of a crate whose lang items changed, once per such write
    let derives: Vec<BuiltinDeriveImplId> = crate_state.derives.keys().copied().collect();
    for imp in derives {
        let key = derive_key(&*db, imp, items);
        let row = crate_state.derives.remove(&imp).expect("a derive impl just listed");
        work.rebuilt += 1;
        if key == row.key {
            crate_state.derives.insert(imp, row);
            continue;
        }
        work.keys += 1;
        arrange_derive(crate_state, imp, &row, -1);
        let row = DeriveRow { key, ..row };
        arrange_derive(crate_state, imp, &row, 1);
        crate_state.derives.insert(imp, row);
    }
}

/// `AssocFn` (design §8): the item names of each trait declared, gone or re-read.
fn apply_traits(
    db: &mut Db,
    state: &mut State,
    crate_state: &mut CrateState,
    delta: &CrateDelta,
    work: &mut Work,
) {
    for &(trait_, weight) in delta.traits.iter() {
        work.ops += 1;
        if weight < 0 {
            set_names(&*db, &mut state.scratch, crate_state, trait_, false, work);
        }
    }
    for &(trait_, weight) in delta.traits.iter() {
        if weight > 0 {
            set_names(&*db, &mut state.scratch, crate_state, trait_, true, work);
        }
    }
    let scratch = &mut state.scratch;
    scratch.redo_traits.clear();
    scratch.redo_traits.extend(delta.touched.iter().filter_map(|it| match *it {
        ModuleDefId::TraitId(it) => Some(it),
        _ => None,
    }));
    let redo = std::mem::take(&mut scratch.redo_traits);
    for &trait_ in &redo {
        if crate_state.assoc.contains_key(&trait_) {
            set_names(&*db, scratch, crate_state, trait_, true, work);
        }
    }
    scratch.redo_traits = redo;
}

/// The traits whose items a macro writes, re-read on a write that changed any binding or def.
fn rename_wild_traits(
    db: &mut Db,
    state: &mut State,
    crate_state: &mut CrateState,
    work: &mut Work,
) {
    if crate_state.wild_traits.is_empty() {
        return;
    }
    let scratch = &mut state.scratch;
    scratch.redo_traits.clear();
    scratch.redo_traits.extend(crate_state.wild_traits.iter().copied());
    let redo = std::mem::take(&mut scratch.redo_traits);
    for &trait_ in &redo {
        work.woken += 1;
        set_names(&*db, scratch, crate_state, trait_, true, work);
    }
    scratch.redo_traits = redo;
}

/// Sets `trait_`'s names to its items' now (`present`) or to none, moving its candidate rows.
fn set_names(
    db: &Db,
    scratch: &mut Scratch,
    crate_state: &mut CrateState,
    trait_: TraitId,
    present: bool,
    work: &mut Work,
) {
    let new = &mut scratch.assoc;
    new.clear();
    if present {
        assoc_names(db, trait_, new);
        if trait_.trait_items(db).macro_calls.is_empty() {
            crate_state.wild_traits.remove(&trait_);
        } else {
            crate_state.wild_traits.insert(trait_);
        }
    } else {
        crate_state.wild_traits.remove(&trait_);
    }
    work.rebuilt += 1;
    let old = crate_state.assoc.remove(&trait_).unwrap_or_default();
    if old == *new {
        if present {
            crate_state.assoc.insert(trait_, old);
        }
        return;
    }
    work.keys += 1;
    scratch.traits_changed.push(trait_);
    for &name in old.iter().filter(|it| new.binary_search(it).is_err()) {
        crate_state.candidates.update(name, trait_, trait_, -1);
    }
    for &name in new.iter().filter(|it| old.binary_search(it).is_err()) {
        crate_state.candidates.update(name, trait_, trait_, 1);
    }
    if present {
        crate_state.assoc.insert(trait_, new.clone()); // clone: a trait's names, inline to eight
    }
}

/// Sets every slice whose rows changed, then the directory's new keys and body modules.
fn publish(
    db: &mut Db,
    state: &mut State,
    crate_state: &mut CrateState,
    krate: Crate,
    fresh: bool,
    work: &mut Work,
) {
    let scratch = &mut state.scratch;
    crate_state.traits.take_changed(&mut scratch.trait_keys);
    crate_state.body_traits.take_changed(&mut scratch.trait_keys);
    scratch.trait_keys.sort_unstable();
    scratch.trait_keys.dedup();
    crate_state.heads.take_changed(&mut scratch.head_keys);
    crate_state.body_heads.take_changed(&mut scratch.head_keys);
    scratch.head_keys.sort_unstable();
    scratch.head_keys.dedup();
    crate_state.candidates.take_changed(&mut scratch.name_keys);
    for trait_ in scratch.trait_keys.drain(..) {
        let rows = trait_rows(&crate_state.traits, &crate_state.body_traits, trait_);
        work.slices += u64::from(crate_state.published.set_trait(db, krate, trait_, rows));
    }
    for head in scratch.head_keys.drain(..) {
        let rows = InherentRows {
            impls: crate_state
                .heads
                .rows(head)
                // clone: each row's position, inline to a depth of six
                .map(|(pos, (module, imp), _)| (module, imp, pos.clone()))
                // alloc: a changed slice's content, once at its length
                .collect(),
            // alloc: as above
            bodies: crate_state.body_heads.get(head).collect(),
        };
        work.slices += u64::from(crate_state.published.set_head(db, krate, head, rows));
    }
    crate_state.body_inherent.take_changed(&mut scratch.units);
    if !scratch.units.is_empty() || fresh {
        scratch.units.clear();
        scratch.body_modules.clear();
        scratch.body_modules.extend(crate_state.body_inherent.get(()));
        crate_state.published.set_body_modules(&scratch.body_modules);
    }
    for name in scratch.name_keys.drain(..) {
        let rows = CandidateRows {
            // alloc: a changed slice's content, once at its length
            traits: crate_state.candidates.get(name).collect(),
        };
        work.slices += u64::from(crate_state.published.set_name(db, krate, name, rows));
    }
    work.slices += crate_state.published.finish(db, krate);
}

/// The rows of `trait_` in position order and its body modules, as a slice holds them.
pub(crate) fn trait_rows(
    traits: &Arrangement<TraitId, Pos, TraitEntry>,
    bodies: &Arrangement<TraitId, ModuleId, ModuleId>,
    trait_: TraitId,
) -> TraitRows {
    let (mut impls, mut derives) = (0, 0);
    for entry in traits.get(trait_) {
        match entry {
            TraitEntry::Impl(..) => impls += 1,
            TraitEntry::Derive(..) => derives += 1,
        }
    }
    // alloc: a changed slice's content, each column once at its length
    let mut impl_rows = Vec::with_capacity(impls);
    // alloc: as above
    let mut derive_rows = Vec::with_capacity(derives);
    for entry in traits.get(trait_) {
        match entry {
            TraitEntry::Impl(module, imp, head) => impl_rows.push((module, imp, head)),
            TraitEntry::Derive(module, imp, head) => derive_rows.push((module, imp, head)),
        }
    }
    TraitRows {
        impls: impl_rows.into_boxed_slice(),
        derives: derive_rows.into_boxed_slice(),
        // alloc: as above
        bodies: bodies.get(trait_).collect(),
    }
}

/// The body parts of the modules a write may have changed the bodies of (design §7): the
/// modules of the defs and impls it touched (a file of theirs written, or their log changed)
/// and of its changed bindings (an item a macro writes), and every module whose bodies may hold
/// an impl, whose impls' paths or macros a change elsewhere may move. A `sweep` (the crate from
/// empty, a crate graph change) or a change of which macros may write impls reads every module.
/// A module no longer in the crate's `DefMap` has an empty part.
fn apply_bodies(
    db: &Db,
    state: &mut State,
    crate_state: &mut CrateState,
    krate: Crate,
    delta: Option<&CrateDelta>,
    sweep: bool,
    work: &mut Work,
) {
    let def_map = crate_def_map(db, krate);
    let macros = macros_writing_impls(db, krate);
    let sweep = sweep || crate_state.macros.as_deref() != Some(macros);
    if crate_state.macros.as_deref() != Some(macros) {
        // alloc: clone: the crate's macro names, once per write that changed them
        crate_state.macros = Some(macros.into());
    }
    let scratch = &mut state.scratch;
    let mut modules = std::mem::take(&mut scratch.body_modules);
    modules.clear();
    if sweep {
        modules.extend(def_map.modules(db).map(|(module, _)| module));
    } else if let Some(delta) = delta {
        modules.extend(delta.touched.iter().filter_map(|def| def.module(db)));
        modules.extend(delta.touched_impls.iter().map(|imp| imp.loc(db).container));
        modules.extend(delta.bindings.iter().map(|&(module, _)| module));
    }
    modules.extend(crate_state.bodies.keys().copied());
    modules.sort_unstable();
    modules.dedup();
    // A part is a query that lowers the module's bodies, and a sweep reads every module's: they
    // are read as one batch on the database's workers, and the loop below diffs their memos in
    // module order.
    if modules.len() > 1 {
        db.run_all(Tag::Infer, modules.len(), |db, at| {
            if def_map.module_rank(db, modules[at]).is_some() {
                _ = body_part(db, modules[at]);
            }
        });
    }
    let empty = BodyPart::default();
    for &module in modules.iter() {
        let now = match def_map.module_rank(db, module) {
            Some(_) => body_part(db, module),
            None => &empty,
        };
        work.woken += 1;
        let old = crate_state.bodies.remove(&module).unwrap_or_default();
        if old == *now {
            if old.holds {
                crate_state.bodies.insert(module, old);
            }
            continue;
        }
        work.keys += 1;
        for (trait_, weight) in moved(&old.traits, &now.traits) {
            crate_state.body_traits.update(trait_, module, module, weight);
        }
        for (head, weight) in moved(&old.heads, &now.heads) {
            crate_state.body_heads.update(head, module, module, weight);
        }
        match (old.heads.is_empty(), now.heads.is_empty()) {
            (true, false) => crate_state.body_inherent.update((), module, module, 1),
            (false, true) => crate_state.body_inherent.update((), module, module, -1),
            _ => {}
        }
        if now.holds {
            // clone: alloc: a changed part, once per module whose part changed
            crate_state.bodies.insert(module, now.clone());
        }
    }
    state.scratch.body_modules = modules;
}

/// The keys of `old` not in `new` (`-1`) and of `new` not in `old` (`+1`), both sorted.
fn moved<'a, K: Ord + Copy>(old: &'a [K], new: &'a [K]) -> impl Iterator<Item = (K, i32)> + 'a {
    let gone = old.iter().filter(|it| new.binary_search(it).is_err()).map(|&it| (it, -1));
    gone.chain(new.iter().filter(|it| old.binary_search(it).is_err()).map(|&it| (it, 1)))
}
