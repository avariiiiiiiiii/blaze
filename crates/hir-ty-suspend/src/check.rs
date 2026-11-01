//! The check: at each call that may park the fiber, every value held across it must be able to
//! cross threads; every vtable and function pointer made keeps the promise of the unmarked
//! trait method or pointer it is called through.
//!
//! A crate is walked from each of its functions with no type or const parameter, through what
//! each instance calls, puts in a vtable or makes a pointer of; each instance reached that is
//! the crate's own, or a generic one instantiated here, is checked. The fiber driver's own
//! bodies (the module of a function that calls a root itself) hold the fiber's handle across
//! its switch by design and are not checked.

use base_db::{Crate, Db};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, HasModule as _, ModuleId,
    attrs::AttrFlags,
    hir::BindingId,
    nameres::demand::{crate_body_owners, walk_bodies},
    signatures::FunctionSignature,
};
use hir_ty::{
    ParamEnvAndCrate,
    next_solver::{DbInterner, GenericArgKind, ParamEnv, StoredTy, Ty, TyKind},
    traits::{StoredParamEnvAndCrate, implements_trait_unique},
};
use hir_ty_mir::mir::{BasicBlockId, LocalId, MirBody, MirSpan, StatementKind, TerminatorKind};
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};
use smallvec::SmallVec;

use crate::{
    dispatch::{Cx, Def, Instance, Target, Unseen, body_of, concrete_args, sites},
    root::{roots, send_trait},
    summary::{owner_of, path, summary, suspends},
};

/// A place in a body: the item whose body it is written in, and where in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Site {
    pub owner: DefWithBodyId,
    pub span: MirSpan,
}

/// Why a value can't be held across a suspend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Why {
    /// Its type is not `Send`.
    NotSend,
    /// Its type is, or holds through a `Box`, tuple, array or reference, this ADT marked
    /// `#[must_not_suspend]`.
    MustNotSuspend(AdtId),
}

/// A value held across a call that may park the fiber, which must not cross threads.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Held {
    /// The call.
    pub call: Site,
    /// The binding that holds it, when one does; else it is a temporary.
    pub binding: Option<BindingId>,
    /// Where it got its value: its binding, or the statement or call that made it.
    pub made: Option<Site>,
    pub ty: StoredTy,
    pub why: Why,
    /// The calls from the call to the root, each named.
    pub path: Box<[String]>,
}

/// What an unmarked `dyn` or pointer call promises (never to suspend), broken where the code
/// that breaks it is made, or a call the walk can't see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Promise {
    /// A vtable of `concrete` whose slot for `method` (`None`: its drop) may suspend, though the
    /// trait method is not `#[blaze::may_suspend]`.
    Dyn { method: Option<FunctionId>, concrete: StoredTy },
    /// A function pointer of a function that may suspend, put somewhere other than a field
    /// marked `#[blaze::may_suspend]`.
    Pointer,
    /// What the instance runs can't be seen.
    Unseen(Unseen),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Broken {
    pub at: Site,
    pub promise: Promise,
    /// The calls from what was made to the root, each named; empty for [`Promise::Unseen`].
    pub path: Box<[String]>,
}

/// What the check found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub held: Box<[Held]>,
    pub broken: Box<[Broken]>,
    /// The crate graph parks fibers but has no `core::marker::Send`: nothing was checked, an
    /// error of its own.
    pub no_send: bool,
}

impl Held {
    /// The path as an error says it: `` `variance` → `fetch` → `get_or_run` → `suspend` ``.
    pub fn path_said(&self) -> String {
        self.path.join(" → ")
    }
}

impl Broken {
    pub fn path_said(&self) -> String {
        self.path.join(" → ")
    }
}

/// What the check finds in `krate`: nothing, and nothing walked, when its crate graph has no
/// root.
#[blaze_query::tracked(returns(ref))]
pub fn suspend_check(db: &Db, krate: Crate) -> Found {
    if roots(db, krate).is_empty() {
        return Found::default();
    }
    if send_trait(db, krate).is_none() {
        return Found { no_send: true, ..Found::default() };
    }
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
    let interner = DbInterner::new_with(db, krate);
    let mut starts = Vec::new(); // alloc: the walk's starts, once per crate
    walk_bodies(db, crate_body_owners(db, krate), |_| true, |owner, _| {
        if let DefWithBodyId::FunctionId(func) = owner
            && FunctionSignature::of(db, func).has_body()
            && let Some(args) = concrete_args(interner, func)
        {
            starts.push(Instance { def: Def::Fn(func), args: args.store() });
        }
    });
    let order = reached(db, &env, &starts);
    let driver: FxHashSet<ModuleId> = order
        .iter()
        .filter(|it| summary(db, env, **it).driver)
        .filter_map(|it| module_of(db, it.def))
        .collect();
    let mut held: Vec<Held> = Vec::new();
    let mut broken: Vec<Broken> = Vec::new();
    let mut seen_held: FxHashSet<(Site, Option<Site>, Why)> = FxHashSet::default();
    let mut seen_broken: FxHashSet<(Site, Promise)> = FxHashSet::default();
    for instance in &order {
        if !checked_here(db, krate, instance) || module_of(db, instance.def).is_some_and(|it| driver.contains(&it)) {
            continue;
        }
        let found = instance_check(db, env, *instance);
        // The same error, found again in another instance of one generic body, is said once.
        for it in found.held.iter() {
            if seen_held.insert((it.call, it.made, it.why)) {
                held.push(it.clone()); // clone: an error, once
            }
        }
        for it in found.broken.iter() {
            if seen_broken.insert((it.at, it.promise)) {
                broken.push(it.clone()); // clone: an error, once
            }
        }
    }
    Found { held: held.into_boxed_slice(), broken: broken.into_boxed_slice(), no_send: false }
}

/// Every instance `starts` reach, in walk order (depth first, each where first reached).
fn reached(db: &Db, env: &StoredParamEnvAndCrate, starts: &[Instance]) -> Vec<Instance> {
    let mut order: Vec<Instance> = Vec::new(); // alloc: the instances, once per crate
    let mut index: FxHashMap<Instance, u32> = FxHashMap::default();
    let mut stack: Vec<(usize, usize)> = Vec::new(); // alloc: the walk's path, once per crate
    for start in starts {
        if index.contains_key(start) {
            continue;
        }
        index.insert(*start, order.len() as u32);
        order.push(*start);
        stack.push((order.len() - 1, 0));
        while let Some(top) = stack.last_mut() {
            let (at, next) = *top;
            let callees = &summary(db, *env, order[at]).callees;
            let Some(callee) = callees.get(next) else {
                stack.pop();
                continue;
            };
            top.1 += 1;
            if !index.contains_key(callee) {
                index.insert(*callee, order.len() as u32);
                order.push(*callee);
                stack.push((order.len() - 1, 0));
            }
        }
    }
    order
}

/// Whether `instance` is checked in `krate`: its body is the crate's own, or it is a generic
/// body instantiated here (with a type or const argument).
fn checked_here(db: &Db, krate: Crate, instance: &Instance) -> bool {
    let own = owner_of(db, instance.def).is_some_and(|it| it.krate(db) == krate);
    own || instance.args.as_ref().iter().any(|it| !matches!(it.kind(), GenericArgKind::Lifetime(_)))
}

fn module_of(db: &Db, def: Def) -> Option<ModuleId> {
    owner_of(db, def).map(|it| it.module(db))
}

/// What the check finds in one instance.
#[blaze_query::tracked(returns(ref))]
fn instance_check(db: &Db, env: StoredParamEnvAndCrate, instance: Instance) -> Found {
    let cx = Cx::new(db, env);
    let Some(owner) = owner_of(db, instance.def) else { return Found::default() };
    let site = |span| Site { owner, span };
    let mut broken = Vec::new(); // alloc: the instance's errors, none when it has none
    match summary(db, cx.stored, instance).unseen {
        // The code generator reports a body that does not lower, as the build's error.
        None | Some(Unseen::NoMir) => {}
        Some(unseen) => broken.push(Broken {
            at: site(MirSpan::Unknown),
            promise: Promise::Unseen(unseen),
            path: Box::default(),
        }),
    }
    let Ok(body) = body_of(db, &cx.stored, &instance) else {
        return Found { broken: broken.into_boxed_slice(), ..Found::default() };
    };
    let sites = sites(&cx, body);

    let mut points: SmallVec<[BasicBlockId; 8]> = SmallVec::new();
    let mut firsts: SmallVec<[&Target; 8]> = SmallVec::new();
    for (block, targets) in &sites.calls {
        if let Some(target) = targets.iter().find(|it| suspends(db, &cx.stored, it)) {
            points.push(*block);
            firsts.push(target);
        }
    }
    let mut held = Vec::new(); // alloc: the instance's errors, none when it has none
    // `suspend_check` reports a graph without `Send` before it checks any instance.
    if !points.is_empty()
        && let Some(send) = send_trait(db, cx.env.krate)
    {
        blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let across = body.held_across_calls_in(frame, &points);
            for ((&block, &target), locals) in points.iter().zip(&firsts).zip(across) {
                let Some(terminator) = &body.basic_blocks[block].terminator else { continue };
                for &local in locals.iter() {
                    let ty = cx.normalize(body.locals[local].ty.as_ref());
                    let why = match must_not_suspend(&cx, ty) {
                        Some(adt) => Why::MustNotSuspend(adt),
                        None if !implements_trait_unique(ty, db, cx.env, *send) => Why::NotSend,
                        None => continue,
                    };
                    let binding = binding_of(body, local);
                    held.push(Held {
                        call: site(terminator.span),
                        binding,
                        made: made_at(body, local, binding).map(site),
                        ty: ty.store(),
                        why,
                        path: path(db, &cx.stored, target),
                    });
                }
            }
        });
    }

    for slot in &sites.slots {
        let marked = slot.method.is_some_and(|it| AttrFlags::query(db, it.into()).contains(AttrFlags::MAY_SUSPEND));
        if marked {
            continue;
        }
        if let Some(target) = slot.targets.iter().find(|it| suspends(db, &cx.stored, it)) {
            broken.push(Broken {
                at: site(slot.span),
                promise: Promise::Dyn { method: slot.method, concrete: slot.concrete.store() },
                path: path(db, &cx.stored, target),
            });
        }
    }
    for reified in &sites.reified {
        if reified.into_marked {
            continue;
        }
        if let Some(target) = reified.targets.iter().find(|it| suspends(db, &cx.stored, it)) {
            broken.push(Broken { at: site(reified.span), promise: Promise::Pointer, path: path(db, &cx.stored, target) });
        }
    }
    Found { held: held.into_boxed_slice(), broken: broken.into_boxed_slice(), no_send: false }
}

/// The ADT marked `#[must_not_suspend]` that `ty` is or holds, as rustc's
/// `check_must_not_suspend_ty` looks: through `Box`, tuples, arrays, slices and references, not
/// into an ADT's fields.
fn must_not_suspend<'db>(cx: &Cx<'db>, ty: Ty<'db>) -> Option<AdtId> {
    match ty.kind() {
        TyKind::Adt(def, args) => {
            let adt = def.def_id();
            if AttrFlags::query(cx.db, adt.into()).contains(AttrFlags::MUST_NOT_SUSPEND) {
                return Some(adt);
            }
            match adt {
                AdtId::StructId(it) if cx.lang.OwnedBox == Some(it) => must_not_suspend(cx, args.type_at(0)),
                _ => None,
            }
        }
        TyKind::Tuple(fields) => fields.iter().find_map(|it| must_not_suspend(cx, it)),
        TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::Ref(_, inner, _) => must_not_suspend(cx, inner),
        _ => None,
    }
}

fn binding_of(body: &MirBody<'_>, local: LocalId) -> Option<BindingId> {
    body.binding_locals.iter().find_map(|(binding, &it)| (it == local).then_some(binding))
}

/// Where `local` got its value: its binding, else the first statement or call that writes it
/// as a whole.
fn made_at(body: &MirBody<'_>, local: LocalId, binding: Option<BindingId>) -> Option<MirSpan> {
    if let Some(binding) = binding {
        return Some(MirSpan::BindingId(binding));
    }
    let whole = |place: &hir_ty_mir::mir::StoredPlace| place.local == local && place.projection.is_empty();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(place, _) = &statement.kind
                && whole(place)
            {
                return Some(statement.span);
            }
        }
        if let Some(terminator) = &block.terminator
            && let TerminatorKind::Call { destination, .. } = &terminator.kind
            && whole(destination)
        {
            return Some(terminator.span);
        }
    }
    None
}
