//! The prepared IR of instances over fixtures: what their shapes share and what the reference
//! walk lists of what the IR builder asks for.

mod db;

use base_db::Db;
use blaze_codegen::{
    Backend, Instance as UnitInstance, Unit,
    backend::{FrontRef, Shape},
    plan::{Mode, OptLevel, Override, Policy, Profile},
    settings::{Coverage, DebugInfo, Panic, Relocation, Sanitize, Sanitizers, Spec, Toolchain},
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
    allocator::{self, Forward},
    backend::{built, compiled_in},
    build::{self, Build},
    places::{self, Placer, place_requests},
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
    fn item(&self, _: &Db, item: ItemId) -> Result<ItemKey, String> {
        Ok(ItemKey(by_id("test item", item)))
    }

    fn code(&self, _: &Db, body: Body) -> Result<CodeHash, String> {
        Ok(CodeHash(by_id("test code", body)))
    }

    fn ty<'db>(&self, _: &'db Db, ty: Ty<'db>) -> Result<ContentId, String> {
        Ok(by_id("test type", ty))
    }

    fn konst<'db>(&self, _: &'db Db, konst: Const<'db>) -> Result<ContentId, String> {
        Ok(by_id("test const", konst))
    }

    fn span(
        &self,
        _: &Db,
        owner: DefWithBodyId,
        span: MirSpan,
    ) -> Result<SpanRef, String> {
        Ok(SpanRef { anchor: by_id("test span", (owner, span)), index: 0 })
    }

    fn declaration(&self, _: &Db, func: FunctionId) -> Result<SpanRef, String> {
        Ok(SpanRef { anchor: by_id("test declaration", func), index: 0 })
    }

    fn file_name(&self, _: &Db, file: base_db::FileId) -> Result<String, String> {
        Ok(format!("{file:?}"))
    }
}

/// The fixture lives as long as the test process, as the frontend borrows it.
fn frontend(text: &str) -> (&'static TestDB, HirFrontend<'static>) {
    held(TestDB::with_single_file(text).0)
}

fn held(db: TestDB) -> (&'static TestDB, HirFrontend<'static>) {
    held_as(db, &policy(OptLevel::O0))
}

fn held_as(db: TestDB, policy: &Policy) -> (&'static TestDB, HirFrontend<'static>) {
    let db: &'static TestDB = Box::leak(Box::new(db));
    let frontend =
        HirFrontend::new(Databases::new(db), Box::new(ById), BuildTime(Default::default()));
    (db, frontend.with_policy(policy))
}

/// A build at `level` of members and dependencies, whose units are as [`spec`]'s.
fn policy(level: OptLevel) -> Policy {
    let toolchain = Toolchain { llvm: Some("test".to_owned()), ..spec().toolchain };
    Policy {
        toolchain,
        profile: Profile {
            opt_level: level,
            overflow_checks: false,
            debug_assertions: false,
            debuginfo: DebugInfo::None,
            panic: Panic::Unwind,
            packages: Default::default(),
            build_override: Override::default(),
        },
        mode: Mode::Output,
        fold: false,
        sanitize: Sanitize::default(),
    }
}

/// Whether a unit of `spec` MIR-inlines, which its references are walked for.
fn inlines(spec: &Spec) -> bool {
    Build::of(spec).inlines
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
        mir_inline: false,
        policy: ContentId::default(),
        sanitize: Sanitize::default(),
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
    db: &Db,
    homes: &mut Homes,
    root: &HirInstance,
    of: FunctionId,
) -> Vec<HirInstance> {
    let is_of = |instance: &HirInstance| {
        matches!(&instance.0, Kind::Code(called, _) if called.def == InstanceDef::Fn(of))
    };
    refs::walk(db, root, false, homes)
        .expect("the walk of the root")
        .into_iter()
        .filter_map(|it| match it {
            FrontRef::Function(instance) if is_of(&instance) => Some(instance),
            _ => None,
        })
        .collect()
}

/// What a backend is given for the references of an instance: one target each.
fn refs_of(
    db: &Db,
    homes: &mut Homes,
    (instance, inlined): (&HirInstance, bool),
) -> Vec<blaze_codegen::Target> {
    let walked = refs::walk(db, instance, inlined, homes).expect("the walk");
    (0..walked.len()).map(|at| blaze_codegen::Target::Extern(format!("ref{at}"))).collect()
}

fn shape(frontend: &HirFrontend, instance: &HirInstance) -> Shape {
    prepared(frontend, &spec(), instance).0
}

fn prepared(frontend: &HirFrontend, spec: &Spec, instance: &HirInstance) -> (Shape, super::Prepared) {
    frontend.with(|db, homes| {
        let refs = refs_of(db, homes, (instance, inlines(spec)));
        let refs: &Refs = &refs;
        let walked = (instance, inlines(spec));
        Backends::default()
            .prepared(db, homes, &*frontend.hashes, &unit(spec), spec, walked, refs)
            .unwrap_or_else(|e| panic!("{instance:?}: {e}"))
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
            let walked = refs::walk(db, &instance, false, homes).expect("the walk");
            assert_eq!(walked, refs::walk(db, &instance, false, homes).expect("the walk again"));
            let mut code = build::with(Build::of(&spec), || built(db, &instance, false))
                .and_then(|it| it)
                .unwrap_or_else(|e| panic!("{instance:?}: {e}"));
            let requests = match &mut code {
                super::Built::Ir(pending) => &mut pending.wants.requests,
                super::Built::Assembled(_, requests) => requests,
            };
            let mut placer = Placer::listed(walked.clone());
            place_requests(db, homes, &ById, compiled_in(db, &instance), &mut placer, requests)
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
        let krate = compiled_in(db, &root);
        let placed = place_requests(db, homes, &ById, krate, &mut placer, &mut pending.wants.requests);
        assert!(placed.is_err());
    });
}

const ALWAYS: &str = r#"
//- /dep.rs crate:dep library
pub fn helper(x: u32) -> u32 { x }
#[inline(always)]
pub fn twice(x: u32) -> u32 { helper(x) }
#[inline(always)]
pub fn outer(x: u32) -> u32 { twice(x) }
//- /main.rs crate:main deps:dep
pub fn roots(x: u32) -> u32 { dep::outer(x) }
"#;

/// An `#[inline(always)]` callee of a crate no edit reaches is built in, as LLVM's
/// always-inliner does at -O0: its place is listed as built in, what its body calls is among
/// the caller's references, and the caller's code names the code of each body built in at
/// any depth.
#[test]
fn an_always_inline_callee_of_a_dependency_is_built_in() {
    let (db, frontend) = held(TestDB::with_files(ALWAYS));
    let root = frontend.root(function(&db, "roots"));
    let spec = spec();
    frontend.with(|db, homes| {
        let walked = refs::walk(db, &root, false, homes).expect("the walk");
        let built_in = refs::built_in(db, &root, false).expect("the walk");
        let [at] = built_in[..] else { panic!("built in: {built_in:?} of {walked:?}") };
        let FrontRef::Function(outer) = &walked[at as usize] else { panic!("{walked:?}") };
        let named = |it: &HirInstance, name: &str| match &it.0 {
            Kind::Code(instance, _) => match instance.def {
                InstanceDef::Fn(func) => FunctionSignature::of(db, func).name.as_str() == name,
                _ => false,
            },
            _ => false,
        };
        assert!(named(outer, "outer"), "{outer:?}");
        let calls_helper = |it: &FrontRef<HirInstance, _>| {
            matches!(it, FrontRef::Function(callee) if named(callee, "helper"))
        };
        assert!(walked.iter().any(calls_helper), "{walked:?}");
        let inlined =
            super::frontend::inlined(db, &ById, (&root, false)).expect("what is built in");
        assert_eq!(inlined.len(), 2, "outer and twice: {inlined:?}");
        let mut code = build::with(Build::of(&spec), || built(db, &root, false))
            .and_then(|it| it)
            .unwrap_or_else(|e| panic!("{root:?}: {e}"));
        let super::Built::Ir(pending) = &mut code else { panic!("asm") };
        let mut placer = Placer::listed(walked);
        place_requests(db, homes, &ById, compiled_in(db, &root), &mut placer, &mut pending.wants.requests)
            .expect("the IR asks only for what the walk lists");
    });
}

const UNEVALUATED: &str = r#"
//- minicore: sized
pub const N: usize = 2 + 2;
pub trait Has { type Held; }
impl Has for u8 { type Held = [u8; N]; }
pub fn takes<T>(x: T) -> T { x }
pub fn roots() {
    takes([0u8; N]);
    takes::<<u8 as Has>::Held>([1u8; 4]);
}
"#;

/// An instance's arguments are as codegen sees them, every alias normalized and every
/// constant evaluated, which is all `Hashes` gives an ID of: `[u8; N]` of a `const N`, and
/// the projection that names it, are `[u8; 4]`.
#[test]
fn an_instances_arguments_are_evaluated() {
    use rustc_type_ir::TypeVisitableExt;
    let (db, frontend) = frontend(UNEVALUATED);
    let (roots, takes) = (function(&db, "roots"), function(&db, "takes"));
    let root = frontend.root(roots);
    let called = frontend.with(|db, homes| called(db, homes, &root, takes));
    assert!(!called.is_empty(), "no instance of `takes`");
    for it in &called {
        let Kind::Code(instance, _) = &it.0 else { panic!("{it:?}") };
        assert!(!instance.args.as_ref().has_aliases(), "{instance:?}");
    }
}

/// A unit's MIR level is rustc's: level 2 for an LLVM unit above -O0 and for the library's
/// crates, else 1; the walk reads level 1, whose bodies ask for all that level 2's do.
#[test]
fn a_units_mir_level_is_rustcs() {
    use hir_ty_mir::mir::MirOptLevel;
    let (db, _) = frontend(SIBLINGS);
    let krate = db.test_crate();
    let level = |spec: &Spec| Build::of(spec).options(db, krate, krate).level;
    let mut spec = spec();
    assert_eq!(level(&spec), MirOptLevel::Debug);
    spec.backend = Backend::Llvm { opt: 0 };
    assert_eq!(level(&spec), MirOptLevel::Debug);
    spec.backend = Backend::Llvm { opt: 2 };
    assert_eq!(level(&spec), MirOptLevel::Release);
    assert_eq!(Build::WALK.options(db, krate, krate).level, MirOptLevel::Debug);
    spec.debug_assertions = true;
    assert!(Build::of(&spec).options(db, krate, krate).ub_checks);
}

/// An entry of the allocator shim is built against what its walk lists, the function it
/// calls by symbol, and is another code for another callee.
#[test]
fn an_allocator_entry_calls_what_its_walk_lists() {
    let (db, frontend) = frontend(REFERRING);
    let krate = db.test_crate();
    let entry = |to| Forward { symbol: "__rust_alloc", to, params: 2, returns: true, krate };
    let ours = HirInstance(Kind::Allocator(entry(Some("__blaze_global_alloc"))));
    let defaults = entry(Some("__rdl_alloc"));
    assert_ne!(allocator::code(&defaults), allocator::code(&entry(Some("__blaze_global_alloc"))));
    assert!(allocator::walk(&entry(None)).is_empty());
    let spec = spec();
    frontend.with(|db, homes| {
        let walked = refs::walk(db, &ours, false, homes).expect("the walk");
        assert_eq!(walked, vec![FrontRef::Extern("__blaze_global_alloc".to_owned())]);
        let mut code = build::with(Build::of(&spec), || built(db, &ours, false))
            .and_then(|it| it)
            .expect("the IR");
        let super::Built::Ir(pending) = &mut code else { panic!("asm") };
        let mut placer = Placer::listed(walked);
        place_requests(db, homes, &ById, compiled_in(db, &ours), &mut placer, &mut pending.wants.requests)
            .expect("what the walk lists");
    });
    // The fixture links no `alloc`: its program has no shim.
    assert!(frontend.allocator_shim(krate).expect("the shim").is_empty());
}

const SANITIZED: &str = r#"
//- minicore: sized, fn
#![feature(sanitize)]
#[sanitize(address = "off")]
pub fn quiet() {}
pub fn loud() {}
#[sanitize(address = "off", thread = "off")]
pub mod inside {
    pub fn inherited() {}
    #[sanitize(thread = "on")]
    pub fn nearest() {}
    pub fn closure() -> u32 { (|| 1)() }
}
#[unsafe(no_mangle)]
pub extern "C" fn __sanitizer_cov_trace_pc_guard(_guard: *mut u32) {}
"#;

fn sanitizing(sanitizers: Sanitizers, coverage: u8) -> Spec {
    let coverage = Coverage { level: coverage, flags: 0 };
    Spec { sanitize: Sanitize { sanitizers, coverage, ..Sanitize::default() }, ..spec() }
}

/// What a function has off of the unit's sanitizers is its nearest `#[sanitize]`, as rustc
/// reads it; a closure has its function's; a `__sanitizer_*` symbol has SanitizerCoverage
/// off; and without a set nothing is off.
#[test]
fn a_functions_sanitizers_off_are_its_nearest_attributes() {
    let (db, frontend) = frontend(SANITIZED);
    let both = Sanitizers::ADDRESS.union(Sanitizers::THREAD);
    let off = |spec: &Spec, instance: &HirInstance| {
        prepared(&frontend, spec, instance).1.function().unsanitized
    };
    let root = |name| frontend.root(function(&db, name));
    let address = sanitizing(Sanitizers::ADDRESS, 0);
    let all = sanitizing(both, 0);
    assert_eq!(off(&address, &root("quiet")), Sanitizers::ADDRESS);
    assert_eq!(off(&address, &root("loud")), Sanitizers::NONE);
    assert_eq!(off(&sanitizing(Sanitizers::THREAD, 0), &root("quiet")), Sanitizers::NONE);
    assert_eq!(off(&spec(), &root("quiet")), Sanitizers::NONE);
    assert_eq!(off(&all, &root("inherited")), both);
    assert_eq!(off(&all, &root("nearest")), Sanitizers::ADDRESS);
    let closure = frontend.with(|db, homes| {
        refs::walk(db, &root("closure"), false, homes)
            .expect("the walk")
            .into_iter()
            .find_map(|it| match it {
                FrontRef::Function(it) => match &it.0 {
                    Kind::Code(code, _) if matches!(code.def, InstanceDef::Closure(_)) => Some(it),
                    _ => None,
                },
                _ => None,
            })
            .expect("the closure")
    });
    assert_eq!(off(&all, &closure), both);
    let callback = root("__sanitizer_cov_trace_pc_guard");
    assert_eq!(off(&sanitizing(Sanitizers::NONE, 2), &callback), Sanitizers::COVERAGE);
    assert_eq!(off(&address, &callback), Sanitizers::NONE);
    assert_eq!(off(&sanitizing(Sanitizers::NONE, 2), &root("loud")), Sanitizers::NONE);
}

/// A function with a sanitizer off is another template than the same IR with it on.
#[test]
fn what_is_unsanitized_is_in_the_shape() {
    let (db, frontend) = frontend(SANITIZED);
    let quiet = frontend.root(function(&db, "quiet"));
    let address = sanitizing(Sanitizers::ADDRESS, 0);
    let thread = sanitizing(Sanitizers::THREAD, 0);
    assert_ne!(prepared(&frontend, &address, &quiet).0, prepared(&frontend, &thread, &quiet).0);
    assert_eq!(prepared(&frontend, &thread, &quiet).0, shape(&frontend, &quiet));
}

/// Cranelift instruments for no sanitizer, so a unit with a set it would build is refused,
/// never built without the checks.
#[test]
fn cranelift_refuses_a_sanitized_unit() {
    use blaze_codegen::backend::Prepare as _;
    let (db, frontend) = frontend(SANITIZED);
    let loud = frontend.root(function(&db, "loud"));
    let refs = frontend.with(|db, homes| refs_of(db, homes, (&loud, false)));
    let refs: &Refs = &refs;
    for spec in [sanitizing(Sanitizers::ADDRESS, 0), sanitizing(Sanitizers::NONE, 1)] {
        let Err(e) = Backends::default().prepare(&frontend, &unit(&spec), &spec, &loud, refs) else {
            panic!("a sanitized Cranelift unit was built");
        };
        assert!(e.to_string().contains("-Zsanitizer and SanitizerCoverage"), "{e}");
    }
    let plain = spec();
    let built = Backends::default().prepare(&frontend, &unit(&plain), &plain, &loud, refs);
    built.expect("a plain unit");
}

/// A unit at LLVM's `opt`, MIR-inlined where the plan inlines one (`Spec::mir_inline`).
fn llvm(opt: u8) -> Spec {
    Spec { backend: Backend::Llvm { opt }, mir_inline: opt >= 2, ..spec() }
}

fn is_call(it: &places::Ref, func: FunctionId) -> bool {
    let FrontRef::Function(HirInstance(Kind::Code(called, _))) = it else { return false };
    called.def == InstanceDef::Fn(func)
}

/// The IR of `instance` built as `spec` says, with what it asked for.
fn built_as(db: &Db, spec: &Spec, instance: &HirInstance) -> super::Built {
    build::with(Build::of(spec), || built(db, instance, false))
        .and_then(|it| it)
        .unwrap_or_else(|e| panic!("{instance:?}: {e}"))
}

/// A unit's bodies are MIR-inlined as rustc's: the library's always, as shipped; every other
/// crate's only in an LLVM unit at level 2 or 3, and in the walk for such a unit.
#[test]
fn a_units_bodies_are_inlined_as_rustcs() {
    let (db, _) = frontend(SIBLINGS);
    let krate = db.test_crate();
    let core = base_db::all_crates(db)
        .iter()
        .copied()
        .find(|it| it.data(db).origin.is_lang())
        .expect("minicore's crate");
    let inline = |build: Build, written| build.options(db, written, krate).inline;
    let backends = [
        (Backend::Cranelift, false),
        (Backend::Llvm { opt: 0 }, false),
        (Backend::Llvm { opt: 1 }, false),
        (Backend::Llvm { opt: 2 }, true),
        (Backend::Llvm { opt: 3 }, true),
    ];
    for (backend, inlines) in backends {
        let build = Build::of(&Spec { backend, mir_inline: inlines, ..spec() });
        assert_eq!(inline(build, krate), inlines, "{backend:?}");
        assert!(inline(build, core), "the library's at {backend:?}");
    }
    // `s`, `z` and a build that patches are at LLVM's level 2 without the inliner.
    let sized = Build::of(&Spec { backend: Backend::Llvm { opt: 2 }, mir_inline: false, ..spec() });
    assert!(!inline(sized, krate));
    assert!(inline(sized, core));
    assert!(!inline(Build::WALK, krate));
    assert!(inline(Build::WALK_INLINED, krate));
}

/// A member at O3 is inlined, at O0 not, and a package the profile sets apart is as the
/// profile says of it; a generic, which any crate may ask for, is walked for inlining where
/// any is inlined.
#[test]
fn which_units_inline_is_the_policys() {
    let (db, frontend) = frontend(SIBLINGS);
    let root = frontend.root(function(db, "roots"));
    let outer = function(db, "outer");
    let generic = frontend.with(|db, homes| called(db, homes, &root, outer)).remove(0);
    let inlines = |policy: &Policy, instance: &HirInstance| {
        let build_time = BuildTime(Default::default());
        let frontend =
            HirFrontend::new(Databases::new(db), Box::new(ById), build_time).with_policy(policy);
        frontend.with(|db, _| frontend.inlines(db, instance)).expect("the policy")
    };
    assert!(!inlines(&policy(OptLevel::O0), &root));
    assert!(inlines(&policy(OptLevel::O3), &root));
    assert!(inlines(&policy(OptLevel::O2), &root));
    assert!(!inlines(&policy(OptLevel::O1), &root));
    let mut set_apart = policy(OptLevel::O3);
    let name = frontend.with(|hir, _| match frontend.origin(hir, db.test_crate()) {
        blaze_codegen::plan::Origin::Member(name) => name,
        other => panic!("the fixture's crate is {other:?}"),
    });
    let o0 = Override { opt_level: Some(OptLevel::O0), ..Override::default() };
    set_apart.profile.packages.insert(name, o0);
    assert!(!inlines(&set_apart, &root));
    // Other members and the library's crates still inline, and any of them may ask for it.
    assert!(inlines(&set_apart, &generic));
    let unwalked =
        HirFrontend::new(Databases::new(db), Box::new(ById), BuildTime(Default::default()));
    let refused = unwalked.with(|db, _| unwalked.inlines(db, &root));
    assert!(refused.is_err_and(|e| e.contains("with_policy")));
}

const INLINED: &str = r#"
//- minicore: sized, copy
#[inline(never)]
pub fn g(x: u32) -> u32 { x ^ 7 }
#[inline]
pub fn f(x: u32) -> u32 { g(x) }
pub fn caller(x: u32) -> u32 { f(x) }
"#;

/// The walk for a unit that MIR-inlines lists what the inlined code calls: `caller` with `f`
/// inlined calls `g`, which the walk at level 1 does not list, as `caller` calls only `f`
/// there. It lists all the walk without inlining does, first.
#[test]
fn the_walk_lists_what_inlined_code_calls() {
    let (db, frontend) = held_as(TestDB::with_single_file(INLINED).0, &policy(OptLevel::O3));
    let root = frontend.root(function(db, "caller"));
    let (f, g) = (function(db, "f"), function(db, "g"));
    frontend.with(|db, homes| {
        assert!(frontend.inlines(db, &root).expect("the policy"));
        let plain = refs::walk(db, &root, false, homes).expect("the walk");
        let walked = refs::walk(db, &root, true, homes).expect("the walk for inlining");
        assert!(plain.iter().any(|it| is_call(it, f)), "{plain:?}");
        assert!(!plain.iter().any(|it| is_call(it, g)), "{plain:?}");
        assert!(walked.iter().any(|it| is_call(it, g)), "{walked:?}");
        assert_eq!(walked[..plain.len()], plain[..]);
        let built_in = refs::built_in(db, &root, true).expect("the walk");
        assert!(built_in.iter().any(|&at| is_call(&walked[at as usize], f)), "{built_in:?}");
        assert!(refs::built_in(db, &root, false).expect("the walk").is_empty());
        let krate = compiled_in(db, &root);
        let mut place = |refs: Vec<places::Ref>| {
            let mut code = built_as(db, &llvm(2), &root);
            let super::Built::Ir(pending) = &mut code else { panic!("asm") };
            let mut placer = Placer::listed(refs);
            place_requests(db, homes, &ById, krate, &mut placer, &mut pending.wants.requests)
                .map(drop)
        };
        assert!(place(plain).is_err(), "the inlined IR asks only for what the plain walk lists");
        place(walked).expect("the inlined IR asks only for what its walk lists");
    });
}

const TWICE: &str = r#"
//- minicore: sized, copy
#[inline]
pub fn leaf(x: u32) -> u32 { x ^ 1 }
#[inline]
pub fn mid(x: u32) -> u32 { leaf(x) ^ 2 }
pub fn caller(x: u32) -> u32 { leaf(x) ^ mid(x) }
"#;

/// A callee inlined directly and through another is listed once, built in once, and its code
/// is among what the caller's key holds once.
#[test]
fn a_callee_inlined_twice_is_built_in_once() {
    let (db, frontend) = held_as(TestDB::with_single_file(TWICE).0, &policy(OptLevel::O3));
    let root = frontend.root(function(db, "caller"));
    let (leaf, mid) = (function(db, "leaf"), function(db, "mid"));
    frontend.with(|db, homes| {
        let walked = refs::walk(db, &root, true, homes).expect("the walk");
        let built_in = refs::built_in(db, &root, true).expect("the walk");
        assert_eq!(walked.iter().filter(|it| is_call(it, leaf)).count(), 1, "{walked:?}");
        let [one, two] = built_in[..] else { panic!("built in: {built_in:?} of {walked:?}") };
        assert_ne!(one, two);
        let of = |func| built_in.iter().any(|&at| is_call(&walked[at as usize], func));
        assert!(of(leaf) && of(mid), "{built_in:?} of {walked:?}");
        let inlined = super::frontend::inlined(db, &ById, (&root, true)).expect("the code");
        assert_eq!(inlined.len(), 2, "leaf and mid: {inlined:?}");
    });
}

const TRACKED: &str = r#"
//- minicore: sized, copy, panic_location
use core::panic::Location;
#[track_caller]
#[inline]
pub fn here() -> &'static Location<'static> { Location::caller() }
pub fn caller() -> &'static Location<'static> { here() }
"#;

/// A `#[track_caller]` callee inlined reports where its caller called it, as it does when it
/// is called: the location is of the caller's body, at the call.
#[test]
fn an_inlined_track_caller_callee_reports_the_call() {
    use crate::fragment::DataRequest;
    let (db, frontend) = held_as(TestDB::with_single_file(TRACKED).0, &policy(OptLevel::O3));
    let root = frontend.root(function(db, "caller"));
    let here = function(db, "here");
    let caller: DefWithBodyId = function(db, "caller").into();
    frontend.with(|db, homes| {
        let walked = refs::walk(db, &root, true, homes).expect("the walk");
        let built_in = refs::built_in(db, &root, true).expect("the walk");
        assert!(built_in.iter().any(|&at| is_call(&walked[at as usize], here)), "{built_in:?}");
        let locations = |spec: &Spec| {
            let code = built_as(db, spec, &root);
            let super::Built::Ir(pending) = &code else { panic!("asm") };
            (pending.wants.requests.datas.iter())
                .filter_map(|it| match it {
                    DataRequest::Location(at) => Some(*at),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let called = locations(&spec());
        let inlined = locations(&llvm(2));
        assert!(matches!(called[..], [Some((owner, _))] if owner == caller), "{called:?}");
        assert_eq!(inlined, called);
    });
}

/// Where each statement is written: the body's own, and inlined code in the body it is of,
/// each scope at its call; own spans read the call the outermost inlined body is at. A
/// statement of a body that is no item's has no place, which is an error.
#[test]
fn inlined_statements_are_written_in_their_callees() {
    use hir_ty_mir::mir::{InlinedScope, InlinedSpan};
    let (db, _) = frontend(TWICE);
    let [owner, leaf, mid] =
        ["caller", "leaf", "mid"].map(|it| DefWithBodyId::from(function(db, it)));
    let expr = |index| {
        MirSpan::ExprId(hir_def::hir::ExprId::from_raw(la_arena::RawIdx::from_u32(index)))
    };
    // `mid` is inlined at 1, and `leaf` into it at its 2.
    let scopes = [
        InlinedScope { callee: 0, call_site: expr(1) },
        InlinedScope { callee: 1, call_site: MirSpan::Inlined(0) },
    ];
    let spans = [InlinedSpan { scope: 0, span: expr(2) }, InlinedSpan { scope: 1, span: expr(3) }];
    let statements = vec![expr(5), MirSpan::Inlined(1), MirSpan::Inlined(0)];
    let callees = vec![Some((mid, None)), Some((leaf, None))];
    let source =
        crate::fragment::Source::resolved(owner, &spans, &scopes, callees, statements.clone())
            .expect("a source");
    assert_eq!(source.spans, [expr(5), expr(1), expr(1)]);
    let written = |at| source.written(at, None).map(|it| (it.owner, it.span));
    assert_eq!(written(0), Some((owner, expr(5))));
    assert_eq!(written(1), Some((leaf, expr(3))));
    assert_eq!(written(2), Some((mid, expr(2))));
    assert_eq!(written(3), None);
    let site = |scope| source.call_site(scope, None).map(|it| (it.owner, it.span));
    assert_eq!(site(0), Some((owner, expr(1))));
    assert_eq!(site(1), Some((mid, expr(2))));
    assert_eq!([0, 1, 2].map(|at| source.scope_of(at)), [None, Some(1), Some(0)]);
    let unplaced = vec![None, Some((leaf, None))];
    let refused = crate::fragment::Source::resolved(owner, &spans, &scopes, unplaced, statements);
    assert!(refused.is_err_and(|e| e.contains("no item's body")));
}
