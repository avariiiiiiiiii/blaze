//! The front end of a v2 build over the database: what each instance and static is by
//! content, whose code it is, and what it refers to.

use std::sync::Mutex;

use base_db::{Crate, CrateOrigin};
use blaze_codegen::{
    CodegenError,
    backend::{Address, Described, DescribedStatic, Frontend},
    plan::Origin,
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
    next_solver::{DbInterner, GenericArgs, ParamEnv, TyKind},
};
use hir_ty_mir::mir::overflow::crate_checks_overflow;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, SliceLike as _};
use salsa::plumbing::FromId;

use super::{Body, BuildTime, Hashes, HirInstance, HirStatic, Kind, refs, undescribed};
use crate::{
    Instance, InstanceDef, exported_static, exported_symbol, fncache, home::Homes,
    linked_symbol,
};

/// Databases for the build's threads: salsa's are `Send`, never `Sync`, so each thread
/// borrows one of its own, forked from the build's.
pub struct Databases {
    idle: Mutex<Vec<Lent>>,
    fork: Box<dyn Fn() -> Box<dyn HirDatabase + Send> + Send + Sync>,
}

struct Lent {
    db: Box<dyn HirDatabase + Send>,
    homes: Homes,
}

impl Databases {
    /// `fork` gives another handle on the build's database, of the same revision.
    pub fn new(fork: impl Fn() -> Box<dyn HirDatabase + Send> + Send + Sync + 'static) -> Self {
        Databases { idle: Mutex::new(Vec::new()), fork: Box::new(fork) }
    }

    pub(crate) fn with<T>(&self, work: impl FnOnce(&dyn HirDatabase, &mut Homes) -> T) -> T {
        let lent = self.idle.lock().unwrap_or_else(|it| it.into_inner()).pop();
        let mut lent = lent.unwrap_or_else(|| Lent { db: (self.fork)(), homes: Homes::default() });
        let out = work(&*lent.db, &mut lent.homes);
        self.idle.lock().unwrap_or_else(|it| it.into_inner()).push(lent);
        out
    }
}

/// The front end of a v2 build.
pub struct HirFrontend {
    pub(crate) databases: Databases,
    pub(crate) hashes: Box<dyn Hashes>,
    pub(crate) build_time: BuildTime,
}

impl HirFrontend {
    pub fn new(databases: Databases, hashes: Box<dyn Hashes>, build_time: BuildTime) -> Self {
        HirFrontend { databases, hashes, build_time }
    }

    /// A function that is not generic, as a root of the build: `main`, a test, an exported
    /// function.
    pub fn root(&self, func: FunctionId) -> HirInstance {
        self.databases.with(|db, homes| {
            let krate = func.krate(db);
            let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
            let instance = Instance {
                def: InstanceDef::Fn(func),
                args: GenericArgs::new_from_slice(&[]).store(),
                checked: false,
            };
            let env = homes.home(db, &instance, &env);
            HirInstance(Kind::Code(instance, env))
        })
    }

    /// The C `main` of a program whose `fn main` is `main`.
    pub fn start(&self, main: FunctionId) -> HirInstance {
        HirInstance(Kind::Start(main))
    }

    pub(crate) fn with<T>(&self, work: impl FnOnce(&dyn HirDatabase, &mut Homes) -> T) -> T {
        self.databases.with(work)
    }

    fn described(
        &self,
        db: &dyn HirDatabase,
        homes: &mut Homes,
        instance: &HirInstance,
        requester: &Origin,
    ) -> Result<Described<HirInstance, HirStatic>, CodegenError> {
        let hashes = &*self.hashes;
        let item = match &instance.0 {
            Kind::Asm(text) => Ok(ItemKey(ContentId::of(ASM_ITEM_DOMAIN, text.as_bytes()))),
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
        let refs = refs::walk(db, instance, homes).map_err(fail)?;
        Ok(Described {
            item,
            code,
            args,
            origin,
            overflow_checks,
            address: if symbol.is_some() { Address::Significant } else { Address::Free },
            symbol,
            refs,
        })
    }

    /// Whose code a crate's is, for its settings.
    pub(crate) fn origin(&self, db: &dyn HirDatabase, krate: Crate) -> Origin {
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
        db: &dyn HirDatabase,
        value: &HirStatic,
    ) -> Result<DescribedStatic<HirInstance, HirStatic>, CodegenError> {
        let hashes = &*self.hashes;
        let item = hashes
            .item(db, ItemId::Static(value.0))
            .map_err(|what| undescribed(value, None, what))?;
        let fail = |what: String| undescribed(value, Some(item), what);
        let compiled = super::backend::static_value(db, hashes, value.0).map_err(fail)?;
        Ok(DescribedStatic {
            item,
            instance: ContentId::default(),
            value: compiled.value,
            origin: self.origin(db, value.0.krate(db)),
            symbol: exported_static(db, value.0).map(|it| it.as_str().to_owned()),
            refs: compiled.refs,
        })
    }
}

impl Frontend for HirFrontend {
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
        self.with(|db, _| self.described_static(db, value))
    }
}

/// The body of a closure or coroutine, by its interned id.
fn closure_loc<'db>(db: &'db dyn HirDatabase, def: InstanceDef) -> Option<InternedClosure<'db>> {
    match def {
        InstanceDef::Closure(id) => Some(InternedClosureId::from_id(id).loc(db)),
        InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
            Some(InternedCoroutineId::from_id(id).loc(db))
        }
        _ => None,
    }
}

fn owner_item(db: &dyn HirDatabase, owner: InferBodyId<'_>) -> Result<ItemId, String> {
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
fn item_of(db: &dyn HirDatabase, kind: &Kind) -> Result<ItemId, String> {
    let (instance, env) = match kind {
        Kind::Code(instance, env) => (instance, env),
        Kind::Start(main) => return Ok(ItemId::Function(*main)),
        Kind::Asm(_) => return Err("an `asm!` function, which is no item".to_owned()),
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
fn code_of(db: &dyn HirDatabase, hashes: &dyn Hashes, kind: &Kind) -> Result<CodeHash, String> {
    let instance = match kind {
        Kind::Code(instance, _) => instance,
        Kind::Asm(text) => return Ok(CodeHash(ContentId::of(ASM_DOMAIN, text.as_bytes()))),
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
fn args_of(db: &dyn HirDatabase, hashes: &dyn Hashes, kind: &Kind) -> Result<ContentId, String> {
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
fn crate_of(db: &dyn HirDatabase, kind: &Kind) -> Option<Crate> {
    match kind {
        Kind::Code(instance, _) => match instance.def {
            InstanceDef::Fn(func) => Some(func.krate(db)),
            def => closure_loc(db, def).map(|loc| loc.owner.module(db).krate(db)),
        },
        Kind::Start(main) => Some(main.krate(db)),
        Kind::Asm(_) => None,
    }
}

/// The overflow checks the MIR of a generic says, as rustc builds a crate's MIR with its own:
/// none in the standard library's, and the instantiating crate's where the body inherits them.
fn written_checks(db: &dyn HirDatabase, kind: &Kind) -> Option<bool> {
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
fn symbol_of(db: &dyn HirDatabase, kind: &Kind) -> Option<String> {
    match kind {
        Kind::Code(instance, _) => match instance.def {
            InstanceDef::Fn(func) if instance.args.as_ref().iter().all(|it| it.region().is_some()) => {
                exported_symbol(db, func).or_else(|| linked_symbol(db, func))
            }
            _ => None,
        },
        Kind::Start(_) => Some(crate::Target::current().entry().0.to_owned()),
        Kind::Asm(_) => None,
    }
}
