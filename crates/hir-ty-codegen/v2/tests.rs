//! The prepared IR of instances over fixtures: what their shapes share and what the reference
//! walk lists of what the IR builder asks for.

mod db;

use blaze_codegen::{
    Backend, Instance as UnitInstance, Unit,
    backend::{FrontRef, Shape},
    settings::{DebugInfo, Panic, Relocation, Spec, Toolchain},
    template::Refs,
};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemId, ItemKey};
use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_ty::{
    db::HirDatabase,
    next_solver::{Const, Ty},
};
use hir_ty_mir::mir::MirSpan;
use test_fixture::WithFixture;

use super::{
    Backends, Body, BuildTime, Databases, Hashes, HirFrontend, HirInstance, Kind,
    backend::built,
    build::{self, Build},
    places::{Placer, place_requests},
    refs,
};
use crate::{InstanceDef, home::Homes};
use db::TestDB;

/// Content IDs of the database's own ids: stable within one database, which is all a test
/// compares.
struct ById;

fn by_id(domain: &'static str, what: impl std::fmt::Debug) -> ContentId {
    ContentId::of(domain, format!("{what:?}").as_bytes())
}

impl Hashes for ById {
    fn item(&self, _: &dyn HirDatabase, item: ItemId) -> Result<ItemKey, String> {
        Ok(ItemKey(by_id("test item", item)))
    }

    fn code(&self, _: &dyn HirDatabase, body: Body) -> Result<CodeHash, String> {
        Ok(CodeHash(by_id("test code", body)))
    }

    fn ty<'db>(&self, _: &'db dyn HirDatabase, ty: Ty<'db>) -> Result<ContentId, String> {
        Ok(by_id("test type", ty))
    }

    fn konst<'db>(&self, _: &'db dyn HirDatabase, konst: Const<'db>) -> Result<ContentId, String> {
        Ok(by_id("test const", konst))
    }

    fn span(
        &self,
        _: &dyn HirDatabase,
        owner: DefWithBodyId,
        span: MirSpan,
    ) -> Result<SpanRef, String> {
        Ok(SpanRef { anchor: by_id("test span", (owner, span)), index: 0 })
    }

    fn declaration(&self, _: &dyn HirDatabase, func: FunctionId) -> Result<SpanRef, String> {
        Ok(SpanRef { anchor: by_id("test declaration", func), index: 0 })
    }

    fn file_name(&self, _: &dyn HirDatabase, file: base_db::FileId) -> Result<String, String> {
        Ok(format!("{file:?}"))
    }
}

/// The fixture lives as long as the test process, as the frontend borrows it.
fn frontend(text: &str) -> (&'static TestDB, HirFrontend<'static>) {
    let db: &'static TestDB = Box::leak(Box::new(TestDB::with_single_file(text).0));
    (db, HirFrontend::new(Databases::new(db), Box::new(ById), BuildTime(Default::default())))
}

#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    let found: Vec<FunctionId> = crate_def_map(db, db.test_crate())
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .filter_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} functions named `{name}`", found.len()),
    }
}

fn spec() -> Spec {
    let target = crate::Target::selected().expect("a target");
    let arch = match target.arch {
        crate::Arch::X86_64 => blaze_codegen::Arch::X86_64,
        crate::Arch::Aarch64 => blaze_codegen::Arch::Aarch64,
        crate::Arch::Wasm32 => blaze_codegen::Arch::Wasm32,
    };
    let pic = crate::target().expect("a code generator").flags().is_pic();
    Spec {
        toolchain: Toolchain {
            id: ContentId::default(),
            target: target.triple().to_owned(),
            arch,
            cpu: target.cpu().to_owned(),
            features: Vec::new(),
            cranelift: "0.136".to_owned(),
            llvm: None,
            frame_pointers: crate::backend::frame_pointers(),
            relocation: if pic { Relocation::Pic } else { Relocation::Static },
        },
        backend: Backend::Cranelift,
        overflow_checks: false,
        debug_assertions: false,
        panic: Panic::Unwind,
        debuginfo: DebugInfo::None,
        fold: false,
        policy: ContentId::default(),
    }
}

fn unit(spec: &Spec) -> Unit {
    Unit {
        instance: UnitInstance {
            code: CodeHash(ContentId::default()),
            args: ContentId::default(),
            settings: spec.settings(),
        },
        backend: spec.backend,
        arch: spec.toolchain.arch,
        item: ItemKey(ContentId::default()),
    }
}

/// The instances of `of` that `root` calls, in the order it calls them.
fn called(
    db: &dyn HirDatabase,
    homes: &mut Homes,
    root: &HirInstance,
    of: FunctionId,
) -> Vec<HirInstance> {
    let is_of = |instance: &HirInstance| {
        matches!(&instance.0, Kind::Code(called, _) if called.def == InstanceDef::Fn(of))
    };
    refs::walk(db, root, homes)
        .expect("the walk of the root")
        .into_iter()
        .filter_map(|it| match it {
            FrontRef::Function(instance) if is_of(&instance) => Some(instance),
            _ => None,
        })
        .collect()
}

/// What a backend is given for the references of an instance: one target each.
fn refs_of(db: &dyn HirDatabase, homes: &mut Homes, instance: &HirInstance) -> Vec<blaze_codegen::Target> {
    let walked = refs::walk(db, instance, homes).expect("the walk");
    (0..walked.len()).map(|at| blaze_codegen::Target::Extern(format!("ref{at}"))).collect()
}

fn shape(frontend: &HirFrontend, instance: &HirInstance) -> Shape {
    let spec = spec();
    frontend.with(|db, homes| {
        let refs = refs_of(db, homes, instance);
        let refs: &Refs = &refs;
        Backends::default()
            .prepared(db, homes, &*frontend.hashes, &unit(&spec), &spec, instance, refs)
            .unwrap_or_else(|e| panic!("{instance:?}: {e}"))
            .0
    })
}

const SIBLINGS: &str = r#"
//- minicore: sized
pub struct A(pub u32);
pub struct B(pub u64);
pub fn inner<T>(x: &T) -> &T { x }
pub fn outer<T>(x: &T) -> &T { inner(x) }
pub fn konst<const N: usize>() -> usize { N }
pub fn roots() {
    outer(&A(1));
    outer(&B(2));
    konst::<1>();
    konst::<2>();
}
"#;

#[test]
fn siblings_over_pointers_have_one_shape() {
    let (db, frontend) = frontend(SIBLINGS);
    let (roots, outer) = (function(&db, "roots"), function(&db, "outer"));
    let root = frontend.root(roots);
    let siblings = frontend.with(|db, homes| called(db, homes, &root, outer));
    let [a, b] = &siblings[..] else { panic!("{siblings:?}") };
    assert_ne!(a, b);
    assert_eq!(shape(&frontend, a), shape(&frontend, b));
}

#[test]
fn different_constants_are_different_shapes() {
    let (db, frontend) = frontend(SIBLINGS);
    let (roots, konst) = (function(&db, "roots"), function(&db, "konst"));
    let root = frontend.root(roots);
    let siblings = frontend.with(|db, homes| called(db, homes, &root, konst));
    let [one, two] = &siblings[..] else { panic!("{siblings:?}") };
    assert_ne!(shape(&frontend, one), shape(&frontend, two));
}

#[test]
fn a_shape_is_the_same_each_time() {
    let (db, frontend) = frontend(SIBLINGS);
    let root = frontend.root(function(&db, "roots"));
    assert_eq!(shape(&frontend, &root), shape(&frontend, &root));
}

const REFERRING: &str = r#"
//- minicore: sized, fn, drop
pub struct Held(pub u32);
impl Drop for Held { fn drop(&mut self) {} }
pub static COUNT: u32 = 3;
extern "C" { pub fn outside(x: u32) -> u32; }
pub fn callee(x: u32) -> u32 { x + COUNT }
pub fn pointer() -> fn(u32) -> u32 { callee }
pub fn generic<T>(x: T) -> T { x }
pub fn roots(x: u32) -> u32 {
    let _held = Held(x);
    let f = pointer();
    let g = |y: u32| y + 1;
    unsafe { outside(f(g(generic(x)))) }
}
"#;

/// The walk lists everything the IR of each instance it reaches asks for: a closed placer
/// over the walk places every request of the IR builder's.
#[test]
fn the_walk_lists_what_the_ir_asks_for() {
    let (db, frontend) = frontend(REFERRING);
    let root = frontend.root(function(&db, "roots"));
    let spec = spec();
    frontend.with(|db, homes| {
        let mut seen = vec![root.clone()];
        let mut at = 0;
        while let Some(instance) = seen.get(at).cloned() {
            at += 1;
            let walked = refs::walk(db, &instance, homes).expect("the walk");
            assert_eq!(walked, refs::walk(db, &instance, homes).expect("the walk again"));
            let mut code = build::with(Build::of(&spec), || built(db, &instance, false))
                .and_then(|it| it)
                .unwrap_or_else(|e| panic!("{instance:?}: {e}"));
            let requests = match &mut code {
                super::Built::Ir(pending) => &mut pending.wants.requests,
                super::Built::Assembled(_, requests) => requests,
            };
            let mut placer = Placer::listed(walked.clone());
            place_requests(db, homes, &ById, &mut placer, requests)
                .unwrap_or_else(|e| panic!("{instance:?} asks for what its walk has not: {e}"));
            for it in walked {
                if let FrontRef::Function(next) = it
                    && matches!(next.0, Kind::Code(..))
                    && !seen.contains(&next)
                {
                    seen.push(next);
                }
            }
        }
        assert!(seen.len() > 3, "{seen:?}");
    });
}

/// A placer closed over a list refuses what is not on it: the IR builder asking for more
/// than the walk said is an error, never a reference added on the way.
#[test]
fn a_reference_left_out_of_the_walk_is_an_error() {
    let (db, frontend) = frontend(REFERRING);
    let root = frontend.root(function(&db, "callee"));
    let spec = spec();
    frontend.with(|db, homes| {
        let mut code = build::with(Build::of(&spec), || built(db, &root, false))
            .and_then(|it| it)
            .expect("the IR");
        let super::Built::Ir(pending) = &mut code else { panic!("asm") };
        let mut placer = Placer::listed(Vec::new());
        let placed = place_requests(db, homes, &ById, &mut placer, &mut pending.wants.requests);
        assert!(placed.is_err());
    });
}
