//! The front end of a v2 build over the database: what each instance and static is by
//! content, whose code it is, and what it refers to.


use base_db::{Db, Crate, CrateOrigin};
use blaze_codegen::{
    CodegenError,
    backend::{Address, Described, DescribedStatic, FrontRef, Frontend},
    plan::{Origin, PlanError, Policy},
};
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemId, ItemKey};
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, FunctionId, HasModule, Lookup, attrs::AttrFlags,
    expr_store::ExpressionStore,
};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::{HirDatabase, InternedClosure, InternedClosureId, InternedCoroutineId},
    next_solver::{DbInterner, GenericArgs, ParamEnv, Region, TyKind},
};
use hir_ty_mir::mir::overflow::crate_checks_overflow;
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, SliceLike as _};
use salsa::plumbing::FromId;
use smallvec::{SmallVec, smallvec};

use super::{Body, BuildTime, HashSource, Hashes, HirInstance, HirStatic, Kind, build::Build, refs, undescribed};
use crate::{
    Instance, InstanceDef, exported_static, exported_symbol, fncache, home::Homes,
    linked_symbol,
};

/// The build's database, `Sync` and shared by reference by every task. Nothing here locks:
/// each call has its own [`Homes`], which lives as long as the task's work on one instance.
pub struct Databases<'db> {
    db: &'db Db,
}

impl<'db> Databases<'db> {
    pub fn new(db: &'db Db) -> Self {
        Databases { db }
    }

    pub(crate) fn with<T>(&self, work: impl FnOnce(&Db, &mut Homes) -> T) -> T {
        work(self.db, &mut Homes::default())
    }
}

/// The check of the bodies a build compiles, run beside it: told what the walk reaches, and
/// whether the build's outcome is already its failure.
pub trait BodyChecks: Sync {
    /// Check these first: bodies the build reached. Any the check does not hold is passed over.
    fn reached(&self, bodies: &[DefWithBodyId]);

    /// Whether a check failed: what the build makes is then not used.
    fn stopped(&self) -> bool;
}

/// The front end of a v2 build.
pub struct HirFrontend<'db> {
    pub(crate) databases: Databases<'db>,
    pub(crate) hashes: HashSource,
    pub(crate) build_time: BuildTime,
    /// Which origins' units MIR-inline: what a function's references are walked for.
    inlining: Option<Inlining>,
    // dyn: called once a level of the walk, never per instance
    checks: Option<&'db dyn BodyChecks>,
}

/// Whether some unit of each origin of the build's [`Policy`] MIR-inlines
/// (`Spec::mir_inline`), the first spec or the one a live build tiers it up to.
struct Inlining {
    /// A member's, a dependency's, build-time code's, of a package the profile names no
    /// override for.
    member: Result<bool, PlanError>,
    dependency: Result<bool, PlanError>,
    build_time: Result<bool, PlanError>,
    /// Of a package the profile names (`[profile.*.package.<name>]`), the same three.
    packages: FxHashMap<String, [Result<bool, PlanError>; 3]>,
    std: Result<bool, PlanError>,
    /// Some origin's units inline: what a generic, which every origin may ask for, is walked
    /// with.
    any: bool,
}

impl Inlining {
    /// Made once a build, choosing for each origin as the plan does.
    fn of(policy: &Policy) -> Inlining {
        let inlines = |origin: Origin| {
            policy.choose(&origin, None).map(|choice| {
                let then = choice.then.is_some_and(|it| Build::of(&it).inlines);
                Build::of(&choice.first).inlines || then
            })
        };
        let kinds = |name: &str| {
            [
                inlines(Origin::Member(name.to_owned())),
                inlines(Origin::Dependency(name.to_owned())),
                inlines(Origin::BuildTime(name.to_owned())),
            ]
        };
        // No package is named the empty name: these are of one the profile has nothing for.
        let [member, dependency, build_time] = kinds("");
        let packages: FxHashMap<_, _> = (policy.profile.packages.keys())
            .filter(|it| *it != "*")
            .map(|name| (name.clone(), kinds(name)))
            .collect();
        let std = inlines(Origin::Std);
        let any = [&member, &dependency, &build_time, &std]
            .into_iter()
            .chain(packages.values().flatten())
            .any(|it| it == &Ok(true));
        Inlining { member, dependency, build_time, packages, std, any }
    }

    fn of_origin(&self, origin: &Origin) -> &Result<bool, PlanError> {
        let (name, kind) = match origin {
            Origin::Std => return &self.std,
            Origin::Member(name) => (name, 0),
            Origin::Dependency(name) => (name, 1),
            Origin::BuildTime(name) => (name, 2),
        };
        match self.packages.get(name) {
            Some(kinds) => &kinds[kind],
            None => [&self.member, &self.dependency, &self.build_time][kind],
        }
    }
}

impl<'db> HirFrontend<'db> {
    pub fn new(databases: Databases<'db>, hashes: HashSource, build_time: BuildTime) -> Self {
        HirFrontend { databases, hashes, build_time, inlining: None, checks: None }
    }

    /// The front end of a build whose unit's bodies `checks` checks beside it.
    // dyn: called once a level of the walk, never per instance
    pub fn with_checks(mut self, checks: &'db dyn BodyChecks) -> Self {
        self.checks = Some(checks);
        self
    }

    /// The front end of a build compiled as `policy` says: what describing and preparing a
    /// function need, which list what its code calls as the units of its origin inline it.
    pub fn with_policy(mut self, policy: &Policy) -> Self {
        self.inlining = Some(Inlining::of(policy));
        self
    }

    /// Whether a unit of `instance` MIR-inlines bodies of crates other than the library's,
    /// whose code then calls what they call: one of its crate's origin when it is not
    /// generic, else of any origin, as any may ask for it. What its references are walked
    /// for ([`refs::walk`]).
    pub(crate) fn inlines(
        &self,
        db: &Db,
        instance: &HirInstance,
    ) -> Result<bool, String> {
        self.inlines_as(db, instance, None)
    }

    /// [`Self::inlines`], with the origin of the instance's crate when the caller has it.
    fn inlines_as(
        &self,
        db: &Db,
        instance: &HirInstance,
        origin: Option<&Origin>,
    ) -> Result<bool, String> {
        let Some(inlining) = &self.inlining else {
            return Err("the front end was made with no policy (`HirFrontend::with_policy`), so \
                        it can't tell which units inline the instance's MIR"
                .to_owned());
        };
        let of_origin = match crate_of(db, &instance.0) {
            Some(krate) if !generic(&instance.0) => match origin {
                Some(origin) => inlining.of_origin(origin),
                None => inlining.of_origin(&self.origin(db, krate)),
            },
            _ => return Ok(inlining.any),
        };
        of_origin.as_ref().copied().map_err(|it| it.to_string())
    }

    /// A function that is not generic, as a root of the build: `main`, a test, an exported
    /// function.
    pub fn root(&self, func: FunctionId) -> HirInstance {
        self.databases.with(|db, homes| root_in(db, homes, func))
    }

    /// The C `main` of a program whose `fn main` is `main`.
    pub fn start(&self, main: FunctionId) -> HirInstance {
        HirInstance(Kind::Start(main))
    }

    /// The allocator shim of the program of `krate`, with the `#[global_allocator]`'s
    /// functions it calls: roots of a program with a `main`, and of a `cdylib`.
    pub fn allocator_shim(&self, krate: Crate) -> Result<Vec<HirInstance>, CodegenError> {
        self.with(|db, _| super::allocator::shim(db, krate, |func| self.root(func)))
            .map_err(|what| undescribed(&krate, None, format!("its allocator shim: {what}")))
    }

    pub(crate) fn with<T>(&self, work: impl FnOnce(&Db, &mut Homes) -> T) -> T {
        self.databases.with(work)
    }

    fn described(
        &self,
        db: &Db,
        homes: &mut Homes,
        instance: &HirInstance,
        requester: &Origin,
    ) -> Result<Described<HirInstance, HirStatic>, CodegenError> {
        let hashes = &self.hashes;
        let item = match &instance.0 {
            Kind::Asm(text) => Ok(ItemKey(ContentId::of(ASM_ITEM_DOMAIN, text.as_bytes()))),
            Kind::Allocator(forward) => Ok(super::allocator::item(forward)),
            kind => item_of(db, kind).and_then(|item| hashes.item(db, item)),
        }
        .map_err(|what| undescribed(instance, None, what))?;
        let fail = |what: String| undescribed(instance, Some(item), what);
        let code = code_of(db, hashes, &instance.0).map_err(fail)?;
        let args = args_of(db, hashes, &instance.0).map_err(fail)?;
        let generic = generic(&instance.0);
        let origin = match crate_of(db, &instance.0) {
            Some(krate) if !generic => self.origin(db, krate),
            _ => requester.clone(),
        };
        let overflow_checks = if generic { written_checks(db, &instance.0) } else { None };
        let symbol = symbol_of(db, &instance.0);
        let inlined = self.inlines_as(db, instance, Some(&origin)).map_err(fail)?;
        let refs = refs::walk(db, instance, inlined, homes).map_err(fail)?;
        let built_in = refs::built_in(db, instance, inlined).map_err(fail)?.to_vec();
        Ok(Described {
            item,
            code,
            args,
            origin,
            overflow_checks,
            address: if symbol.is_some() { Address::Significant } else { Address::Free },
            symbol,
            name: super::linked::debug_name(db, &instance.0),
            refs,
            built_in,
        })
    }

    /// Whose code a crate's is, for its settings.
    pub(crate) fn origin(&self, db: &Db, krate: Crate) -> Origin {
        let data = krate.data(db);
        let name = match &data.origin {
            CrateOrigin::Lang(_) => return Origin::Std,
            CrateOrigin::Local { name: Some(name), .. } | CrateOrigin::Library { name, .. } => {
                name.as_str().to_owned()
            }
            CrateOrigin::Local { name: None, .. } | CrateOrigin::Rustc { .. } => {
                fncache::crate_name(db, krate)
            }
        };
        if self.build_time.0.contains(&krate) {
            return Origin::BuildTime(name);
        }
        match data.origin {
            CrateOrigin::Local { .. } => Origin::Member(name),
            _ => Origin::Dependency(name),
        }
    }

    fn described_static(
        &self,
        db: &Db,
        homes: &mut Homes,
        value: &HirStatic,
    ) -> Result<DescribedStatic<HirInstance, HirStatic>, CodegenError> {
        let hashes = &self.hashes;
        let item = hashes
            .item(db, ItemId::Static(value.0))
            .map_err(|what| undescribed(value, None, what))?;
        let held = super::statics::value(db, homes, hashes, value.0)
            .map_err(|what| undescribed(value, Some(item), what))?;
        Ok(DescribedStatic {
            item,
            instance: ContentId::default(),
            value: held.value,
            mutable: held.mutable,
            thread_local: held.thread_local,
            section: held.section,
            origin: self.origin(db, value.0.krate(db)),
            symbol: exported_static(db, value.0).map(|it| it.as_str().to_owned()),
            refs: held.refs,
        })
    }
}

impl Frontend for HirFrontend<'_> {
    type Instance = HirInstance;
    type Static = HirStatic;

    fn describe(
        &self,
        instance: &HirInstance,
        requester: &Origin,
    ) -> Result<Described<HirInstance, HirStatic>, CodegenError> {
        self.with(|db, homes| self.described(db, homes, instance, requester))
    }

    fn describe_static(
        &self,
        value: &HirStatic,
    ) -> Result<DescribedStatic<HirInstance, HirStatic>, CodegenError> {
        self.with(|db, homes| self.described_static(db, homes, value))
    }

    fn reached(&self, functions: &[(HirInstance, Origin)], statics: &[HirStatic]) {
        let Some(checks) = self.checks else { return };
        let bodies: Vec<DefWithBodyId> = self.with(|db, _| {
            (functions.iter().filter_map(|(instance, _)| body_of(db, instance)))
                .chain(statics.iter().map(|it| DefWithBodyId::StaticId(it.0)))
                // alloc: once a level of the walk, sized by it
                .collect()
        });
        checks.reached(&bodies);
    }

    fn stopped(&self) -> bool {
        self.checks.is_some_and(|it| it.stopped())
    }
}

/// The body whose check an instance's code waits on: a function's, or that of the item a
/// closure or coroutine is written in. `None` for a shim, `asm!`, the allocator's and a closure
/// in an anonymous constant, which their module's item checks cover.
fn body_of(db: &Db, instance: &HirInstance) -> Option<DefWithBodyId> {
    match &instance.0 {
        Kind::Code(instance, _) => match instance.def {
            InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => Some(func.into()),
            def @ (InstanceDef::Closure(_)
            | InstanceDef::Coroutine(_)
            | InstanceDef::CoroutineByMove(_)) => match closure_loc(db, def)?.owner {
                InferBodyId::DefWithBodyId(owner) => Some(owner),
                InferBodyId::AnonConstId(_) => None,
            },
            InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => None,
        },
        Kind::Start(main) => Some((*main).into()),
        Kind::Asm(_) | Kind::Allocator(_) => None,
    }
}

/// [`HirFrontend::root`] in a task's `homes`. Each lifetime parameter is erased, as in the
/// instance a call names, so the root and the call are one instance.
pub(crate) fn root_in(db: &Db, homes: &mut Homes, func: FunctionId) -> HirInstance {
    let krate = func.krate(db);
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
    let interner = DbInterner::new_no_crate(db);
    let erased = Region::new_erased(interner);
    let instance = Instance {
        def: InstanceDef::Fn(func),
        args: GenericArgs::for_item(interner, func.into(), |_, _, _, _| erased.into()).store(),
        checked: false,
    };
    let env = homes.home(db, &instance, &env);
    HirInstance(Kind::Code(instance, env))
}

/// The code hashes of the instances whose bodies the IR of `instance` builds in, at any depth,
/// sorted, each once: its [`blaze_codegen::CompiledFunction::inlined`]. Nothing is allocated
/// for one that builds nothing in, as nearly every one. `inlined` is whether its references
/// were walked for a unit that MIR-inlines ([`HirFrontend::inlines`]), and so the callees'
/// are: what was inlined into a callee was inlined with it.
pub(crate) fn inlined<'a>(
    db: &'a Db,
    hashes: &HashSource,
    (instance, inlined): (&'a HirInstance, bool),
) -> Result<Vec<CodeHash>, String> {
    let mut found = Vec::new();
    let mut queue: SmallVec<[&HirInstance; 8]> = smallvec![instance];
    let mut seen = FxHashSet::default();
    while let Some(at) = queue.pop() {
        let built_in = refs::built_in(db, at, inlined)?;
        if built_in.is_empty() {
            continue;
        }
        let refs = refs::walked(db, at, inlined)?;
        for &place in built_in {
            let Some(FrontRef::Function(callee)) = refs.get(place as usize) else {
                return Err(format!("{at:?} builds in place {place}, which is no function"));
            };
            if seen.insert(callee) {
                found.push(code_of(db, hashes, &callee.0)?);
                queue.push(callee);
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

/// The body of a closure or coroutine, by its interned id.
fn closure_loc<'db>(db: &'db Db, def: InstanceDef) -> Option<InternedClosure<'db>> {
    match def {
        InstanceDef::Closure(id) => Some(InternedClosureId::from_id(id).loc(db)),
        InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
            Some(InternedCoroutineId::from_id(id).loc(db))
        }
        _ => None,
    }
}

fn owner_item(db: &Db, owner: InferBodyId<'_>) -> Result<ItemId, String> {
    match owner {
        InferBodyId::DefWithBodyId(owner) => Ok(match owner {
            DefWithBodyId::FunctionId(it) => ItemId::Function(it),
            DefWithBodyId::StaticId(it) => ItemId::Static(it),
            DefWithBodyId::ConstId(it) => ItemId::Const(it),
            DefWithBodyId::VariantId(it) => ItemId::Adt(AdtId::EnumId(it.lookup(db).parent)),
        }),
        InferBodyId::AnonConstId(_) => {
            Err("a closure in an anonymous constant, which is no item".to_owned())
        }
    }
}

/// Which item an instance is of. A shim is of the trait method or lang item rustc names it
/// by: drop glue is `drop_in_place`, a vtable's `call_once` is `FnOnce::call_once`.
pub(super) fn item_of(db: &Db, kind: &Kind) -> Result<ItemId, String> {
    let (instance, env) = match kind {
        Kind::Code(instance, env) => (instance, env),
        Kind::Start(main) => return Ok(ItemId::Function(*main)),
        Kind::Asm(_) => return Err("an `asm!` function, which is no item".to_owned()),
        Kind::Allocator(_) => {
            return Err("a function of the allocator shim, which is no item".to_owned());
        }
    };
    let lang_items = || DbInterner::new_with(db, env.krate).lang_items();
    let lang = |item: Option<FunctionId>, name: &str| {
        item.map(ItemId::Function).ok_or_else(|| format!("no `{name}` lang item"))
    };
    match instance.def {
        InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => Ok(ItemId::Function(func)),
        InstanceDef::Closure(_) | InstanceDef::Coroutine(_) | InstanceDef::CoroutineByMove(_) => {
            let loc = closure_loc(db, instance.def).expect("a closure or coroutine");
            owner_item(db, loc.owner)
        }
        InstanceDef::DropGlue => lang(lang_items().DropInPlace, "drop_in_place"),
        InstanceDef::CallOnceShim | InstanceDef::FnPointerShim => {
            lang(lang_items().FnOnce_call_once, "FnOnce::call_once")
        }
        InstanceDef::ReifyShim { .. } => {
            match instance.args.as_ref().type_at(0).kind() {
                TyKind::FnDef(def, _) => Ok(match def.0 {
                    CallableDefId::FunctionId(it) => ItemId::Function(it),
                    CallableDefId::StructId(it) => ItemId::Adt(AdtId::StructId(it)),
                    CallableDefId::EnumVariantId(it) => {
                        ItemId::Adt(AdtId::EnumId(it.lookup(db).parent))
                    }
                }),
                _ => lang(lang_items().FnOnce_call_once, "FnOnce::call_once"),
            }
        }
    }
}

const SHIM_DOMAIN: &str = "blaze v2 shim 1";
/// An `asm!` function is no item: it is named by its text.
const ASM_ITEM_DOMAIN: &str = "blaze v2 asm item 1";
const ASM_DOMAIN: &str = "blaze v2 asm 1";
const START_DOMAIN: &str = "blaze v2 start 1";
const ARGS_DOMAIN: &str = "blaze v2 args 1";

/// What an instance's code is. A shim's is what kind of shim it is, with the code of the
/// function whose signature it adapts: what else it reads is its type argument's.
fn code_of(db: &Db, hashes: &HashSource, kind: &Kind) -> Result<CodeHash, String> {
    let instance = match kind {
        Kind::Code(instance, _) => instance,
        Kind::Asm(text) => return Ok(CodeHash(ContentId::of(ASM_DOMAIN, text.as_bytes()))),
        Kind::Allocator(forward) => return Ok(super::allocator::code(forward)),
        Kind::Start(main) => {
            let mut hasher = Hasher::new(START_DOMAIN);
            hasher.id(&hashes.code(db, Body::Item((*main).into()))?.0);
            return Ok(CodeHash(hasher.finish()));
        }
    };
    let shim = |tag: u8, adapted: Option<FunctionId>| -> Result<CodeHash, String> {
        let mut hasher = Hasher::new(SHIM_DOMAIN);
        hasher.update(&[tag]);
        if let Some(func) = adapted {
            hasher.id(&hashes.code(db, Body::Item(func.into()))?.0);
        }
        Ok(CodeHash(hasher.finish()))
    };
    match instance.def {
        InstanceDef::Fn(func) => hashes.code(db, Body::Item(func.into())),
        InstanceDef::Closure(id) => hashes.code(db, Body::Closure(id)),
        InstanceDef::Coroutine(id) => hashes.code(db, Body::Coroutine(id)),
        InstanceDef::CoroutineByMove(id) => hashes.code(db, Body::CoroutineByMove(id)),
        InstanceDef::DropGlue => shim(0, None),
        InstanceDef::CallOnceShim => shim(1, None),
        InstanceDef::VtableShim(func) => shim(2, Some(func)),
        InstanceDef::FnPointerShim => shim(3, None),
        InstanceDef::ReifyShim { receiver } => {
            let adapted = match instance.args.as_ref().type_at(0).kind() {
                TyKind::FnDef(def, _) => match def.0 {
                    CallableDefId::FunctionId(func) => Some(func),
                    CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => None,
                },
                _ => None,
            };
            shim(if receiver { 5 } else { 4 }, adapted)
        }
    }
}

/// The content ID of an instance's generic arguments, regions left out.
fn args_of(db: &Db, hashes: &HashSource, kind: &Kind) -> Result<ContentId, String> {
    let Kind::Code(instance, _) = kind else { return Ok(ContentId::default()) };
    let mut hasher = Hasher::new(ARGS_DOMAIN);
    for arg in instance.args.as_ref().iter() {
        if let Some(ty) = arg.ty() {
            hasher.update(&[0]).id(&hashes.ty(db, ty)?);
        } else if let Some(konst) = arg.konst() {
            hasher.update(&[1]).id(&hashes.konst(db, konst)?);
        }
    }
    Ok(hasher.finish())
}

/// Whether the instance is compiled as the crate asking for it says: it has generic
/// arguments other than regions, as every shim does.
fn generic(kind: &Kind) -> bool {
    let Kind::Code(instance, _) = kind else { return false };
    match instance.def {
        InstanceDef::Fn(_)
        | InstanceDef::Closure(_)
        | InstanceDef::Coroutine(_)
        | InstanceDef::CoroutineByMove(_) => {
            instance.args.as_ref().iter().any(|arg| arg.region().is_none())
        }
        InstanceDef::DropGlue
        | InstanceDef::CallOnceShim
        | InstanceDef::VtableShim(_)
        | InstanceDef::FnPointerShim
        | InstanceDef::ReifyShim { .. } => true,
    }
}

/// The crate an instance's code is written in, for one that is not generic.
pub(super) fn crate_of(db: &Db, kind: &Kind) -> Option<Crate> {
    match kind {
        Kind::Code(instance, _) => match instance.def {
            InstanceDef::Fn(func) => Some(func.krate(db)),
            def => closure_loc(db, def).map(|loc| loc.owner.module(db).krate(db)),
        },
        Kind::Start(main) => Some(main.krate(db)),
        Kind::Asm(_) | Kind::Allocator(_) => None,
    }
}

/// The overflow checks the MIR of a generic says, as rustc builds a crate's MIR with its own:
/// none in the standard library's, and the instantiating crate's where the body inherits them.
fn written_checks(db: &Db, kind: &Kind) -> Option<bool> {
    let Kind::Code(instance, _) = kind else { return None };
    let (krate, inherits) = match instance.def {
        InstanceDef::Fn(func) => (
            func.krate(db),
            AttrFlags::query(db, func.into()).contains(AttrFlags::RUSTC_INHERIT_OVERFLOW_CHECKS),
        ),
        def => {
            let loc = closure_loc(db, def)?;
            let store = ExpressionStore::of(db, loc.owner.expression_store_owner(db));
            (loc.owner.module(db).krate(db), store.inherits_overflow_checks(loc.expr))
        }
    };
    if inherits {
        return None;
    }
    Some(crate_checks_overflow(db, krate).unwrap_or(false))
}

/// The symbol outside code names an instance by: `#[no_mangle]`, `#[export_name]`, the
/// standard library's functions linked by name, the C `main`.
pub(super) fn symbol_of(db: &Db, kind: &Kind) -> Option<String> {
    match kind {
        Kind::Code(instance, _) => match instance.def {
            InstanceDef::Fn(func) if instance.args.as_ref().iter().all(|it| it.region().is_some()) => {
                exported_symbol(db, func).or_else(|| linked_symbol(db, func))
            }
            _ => None,
        },
        Kind::Start(_) => Some(crate::Target::current().entry().0.to_owned()),
        Kind::Allocator(forward) => Some(forward.symbol.to_owned()),
        Kind::Asm(_) => None,
    }
}
