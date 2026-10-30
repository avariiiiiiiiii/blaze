//! Answering an edit without writing the whole object again: the object of the last full build
//! stays as it is, and a small one with the functions that changed is linked ahead of it.
//!
//! The base gives its instances and statics names the patch can refer to. What the patch
//! defines under a name of the base replaces it there, for the linker takes the first
//! definition it sees. Whatever the patch can't express starts a full build over.

#![forbid(unsafe_code)]

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use base_db::{Db, Crate, FileId};
use cranelift_module::{Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use either::Either;
use hir_def::{FunctionId, HasModule, Lookup, StaticId, signatures::FunctionSignature};
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
use smallvec::SmallVec;

type FxIndexMap<K, V> = indexmap::IndexMap<K, V, FxBuildHasher>;

use hir_ty::{traits::StoredParamEnvAndCrate};

use super::{
    CodegenError, Instance, InstanceDef, ObjectCodegen, OwnStatics, Phases, Rejections,
    Result, Roots, SymbolResolver, TheirStatics, Walked, async_blocks,
    compiled_instance, compiled_root, compiled_static, declaration_location,
    fragment::{CompiledFunction, CompiledStatic, DataRequest, FuncTarget, Requests},
    home::{body_file, body_files, mentioned_crates, written_in},
    instance_serial, is_thread_local, left_to_rustc, root_serial, row_location, source_location,
    static_serial, target,
};

/// What a compiled function or static was when the base took it in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Mark {
    serial: u128,
    /// The source locations it reports, which stitching fills in.
    locations: u64,
    /// The lines its code is of, when a debugger is told them.
    lines: u64,
}

impl Mark {
    fn new(db: &Db, serial: u128, requests: &Requests) -> Mark {
        let mut hasher = rustc_hash::FxHasher::default();
        for request in &requests.datas {
            match request {
                DataRequest::Location(Some((owner, span))) => {
                    source_location(db, *owner, *span).hash(&mut hasher)
                }
                DataRequest::Declaration(func) => declaration_location(db, *func).hash(&mut hasher),
                _ => {}
            }
        }
        Mark { serial, locations: hasher.finish(), lines: 0 }
    }

    /// The mark of a function, whose code is a patch's to describe again when `debug` is set
    /// and a line of it is another: an edit above it in the file moves them all.
    fn of_function(
        db: &Db,
        serial: u128,
        compiled: &CompiledFunction,
        debug: bool,
    ) -> Mark {
        let mut hasher = rustc_hash::FxHasher::default();
        if debug && let Some(source) = &compiled.source {
            for row in &compiled.code.lines {
                let written = source.written(row.span, compiled.closure);
                written.map(|it| row_location(db, it.owner, it.closure, it.span)).hash(&mut hasher);
            }
        }
        Mark { lines: hasher.finish(), ..Mark::new(db, serial, &compiled.requests) }
    }

    /// The mark of a root, `None` when it does not compile.
    fn of_root(db: &Db, root: FunctionId, debug: bool) -> Option<Mark> {
        if debug {
            return root_mark(db, root);
        }
        let compiled = compiled_root(db, root).as_ref().ok()?;
        Some(Mark::of_function(db, root_serial(db, root), compiled, false))
    }

    /// The mark of an instance, `None` when it does not compile.
    fn of_instance(
        db: &Db,
        instance: &Instance,
        env: &StoredParamEnvAndCrate,
        debug: bool,
    ) -> Option<Mark> {
        if debug {
            return instance_mark(db, *instance, *env);
        }
        let compiled = compiled_instance(db, *instance, *env).as_ref().ok()?;
        let serial = instance_serial(db, *instance, *env);
        Some(Mark::of_function(db, serial, compiled, false))
    }

    /// Whether the code is the same, wherever in the file it is: a copy of rustc's that no
    /// patch reaches does what it did, and only a debugger is told the lines it was at.
    fn same_code(self, other: Mark) -> bool {
        Mark { lines: 0, ..self } == Mark { lines: 0, ..other }
    }
}

/// The mark of a root with the lines of its code. Every edit asks it of every root the edit
/// can reach, and found again it would ask where each row of the code is; kept, asking again
/// verifies the places it read, which instances of one function share.
#[salsa::tracked(returns(copy))]
fn root_mark(db: &Db, root: FunctionId) -> Option<Mark> {
    let compiled = compiled_root(db, root).as_ref().ok()?;
    Some(Mark::of_function(db, root_serial(db, root), compiled, true))
}

/// [`root_mark`], of an instance.
#[salsa::tracked(returns(copy))]
fn instance_mark(
    db: &Db,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Option<Mark> {
    let compiled = compiled_instance(db, instance, env).as_ref().ok()?;
    let serial = instance_serial(db, instance, env);
    Some(Mark::of_function(db, serial, compiled, true))
}

/// Something in the base that an edit can change or take away.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Node {
    Root(FunctionId),
    Instance(Instance),
    Static(StaticId),
}

impl Node {
    fn is_asked_for_by(&self, requests: &Requests) -> bool {
        match self {
            Node::Root(_) => false,
            Node::Instance(instance) => requests.funcs.iter().any(
                |it| matches!(&it.target, FuncTarget::Instance(asked, _) if asked == instance),
            ),
            Node::Static(id) => requests
                .datas
                .iter()
                .any(|it| matches!(it, DataRequest::Static(asked) if asked == id)),
        }
    }
}

/// What a query gives for something of the base, or `None` when it panics: the edit may have
/// taken away what the base knew it by, and only looking tells. A build that is interrupted
/// goes on unwinding: that says nothing about the base. One that waited for a thread whose
/// own look panicked has found what that thread found.
fn probe<T>(query: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(query)) {
        Ok(found) => Some(found),
        Err(stopped) => match stopped.downcast_ref::<salsa::Cancelled>() {
            Some(salsa::Cancelled::PropagatedPanic) | None => None,
            Some(_) => std::panic::resume_unwind(stopped),
        },
    }
}

/// Whether `root` has a type or constant to be given, of its own or of its `impl` or trait
/// (`Self` of a trait's method).
pub(super) fn copied_by_rustc(db: &Db, root: FunctionId) -> bool {
    let generics = hir_ty::generics::generics(db, root.into());
    generics.iter(false).any(|(id, _)| !matches!(id, hir_def::GenericParamId::LifetimeParamId(_)))
}

/// What a closure of the body of `root`, which is not generic, is compiled in.
fn closure_env(db: &Db, root: FunctionId) -> StoredParamEnvAndCrate {
    let param_env = hir_ty::next_solver::ParamEnv::empty();
    hir_ty::traits::ParamEnvAndCrate { param_env, krate: root.krate(db) }.store()
}

fn closure_mark(
    db: &Db,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
    debug: bool,
) -> Option<Mark> {
    Mark::of_instance(db, instance, env, debug)
}

#[derive(Clone)]
pub(super) struct BaseInstance {
    pub(super) env: StoredParamEnvAndCrate,
    pub(super) symbol: String,
    /// `None` when the base calls rustc's copy and compiling the instance failed.
    mark: Option<Mark>,
    forwarded: bool,
    /// The files whose bodies it comes from, or `None` when any may be one.
    files: Option<Box<[FileId]>>,
    /// What in the base refers to it.
    parents: Vec<Node>,
    /// The functions left to rustc reach it.
    theirs: bool,
}

/// An instance the functions left to rustc reach that the base has no code for.
#[derive(Clone)]
struct TheirInstance {
    instance: Instance,
    env: StoredParamEnvAndCrate,
    serial: Option<u128>,
    files: Option<Box<[FileId]>>,
}

/// A static the functions left to rustc reach that is rustc's.
#[derive(Clone)]
struct TheirStatic {
    id: StaticId,
    serial: Option<u128>,
    krate: Crate,
    file: FileId,
}

/// What the walk for [`TheirStatics`] went through, apart from the functions it starts at.
/// What it finds only changes with the code of one of these.
#[derive(Clone, Default)]
struct TheirWalk {
    instances: Vec<TheirInstance>,
    statics: Vec<TheirStatic>,
}

impl TheirWalk {
    /// Marks what of the base the walk went through, and keeps the rest.
    fn new(
        db: &Db,
        found: &TheirStatics,
        walked: Walked,
        instances: &mut FxIndexMap<Instance, BaseInstance>,
        statics: &mut FxHashMap<StaticId, BaseStatic>,
    ) -> TheirWalk {
        instances.values_mut().for_each(|it| it.theirs = false);
        statics.values_mut().for_each(|it| it.theirs = false);
        let mut walk = TheirWalk::default();
        let mut seen = FxHashSet::default();
        for (instance, env) in walked {
            if let Some(known) = instances.get_mut(&instance)
                && known.env == env
            {
                known.theirs = true;
                continue;
            }
            if !seen.insert((instance, env)) {
                continue;
            }
            let compiled = compiled_instance(db, instance, env).as_ref().ok();
            walk.instances.push(TheirInstance {
                serial: compiled.map(|_| instance_serial(db, instance, env)),
                files: body_files(db, &instance),
                instance,
                env,
            });
        }
        for &id in &found.statics {
            if let Some(known) = statics.get_mut(&id) {
                known.theirs = true;
                continue;
            }
            walk.statics.push(TheirStatic {
                id,
                serial: compiled_static(db, id).as_ref().ok().map(|_| static_serial(db, id)),
                krate: id.krate(db),
                file: body_file(db, id.into()),
            });
        }
        walk
    }
}

/// A function rustc's copy of a function left to rustc calls rustc's copy of: an instance of
/// a generic one, which rustc puts in the crate that calls it, or one no copy of ours takes
/// the place of. rustc's copies are of the source the daemon started from, so the copy of
/// the function left to rustc is only good while each of these is what it was then.
#[derive(Clone)]
struct Reached {
    node: Node,
    env: StoredParamEnvAndCrate,
    /// The function left to rustc it was reached from.
    by: FunctionId,
    krate: Crate,
    files: Option<Box<[FileId]>>,
    /// Whether what it calls is among the others: one that does not compile calls nobody
    /// knows what.
    walked: bool,
}

impl Reached {
    /// From the functions left to rustc through what of rustc's they reach, stopping at a
    /// function of ours: rustc's code calls it by its symbol, which ours takes over. The
    /// functions of crates the build has none of are only rustc's, and no edit reaches them.
    fn walk(
        db: &Db,
        from_rustc: &[FunctionId],
        compiled: &[(FunctionId, String)],
        skipped: &[(FunctionId, CodegenError)],
    ) -> Vec<Reached> {
        // alloc: once per full build, looked up per instance the walk reaches
        let ours: FxHashSet<FunctionId> = compiled.iter().map(|it| it.0).collect();
        let crates: FxHashSet<Crate> = compiled
            .iter()
            .map(|it| it.0)
            .chain(skipped.iter().map(|it| it.0))
            .map(|it| it.krate(db))
            // alloc: once per full build, as `ours`
            .collect();
        // alloc: once per full build: the walk's stack, and the result, kept in the base
        let (mut stack, mut reached) = (Vec::new(), Vec::new());
        for &by in from_rustc {
            match compiled_root(db, by) {
                Ok(compiled) => stack.push((&compiled.requests, by)),
                Err(_) => reached.push(Reached {
                    node: Node::Root(by),
                    env: closure_env(db, by),
                    by,
                    krate: by.krate(db),
                    files: None,
                    walked: false,
                }),
            }
        }
        let mut seen = FxHashSet::default();
        while let Some((requests, by)) = stack.pop() {
            for request in &requests.funcs {
                let FuncTarget::Instance(instance, env) = &request.target else { continue };
                if !seen.insert((*instance, *env)) {
                    continue;
                }
                let (node, krate, files, requests) = match instance.def {
                    InstanceDef::Fn(func) if instance.args.as_ref().is_empty() => {
                        if ours.contains(&func) || !crates.contains(&func.krate(db)) {
                            continue;
                        }
                        let compiled = compiled_root(db, func).as_ref().ok();
                        let files = Some(Box::from([body_file(db, func.into())]));
                        (Node::Root(func), func.krate(db), files, compiled.map(|it| &it.requests))
                    }
                    _ => {
                        let compiled = compiled_instance(db, *instance, *env);
                        let krate = written_in(db, instance).map_or(env.krate, |it| it.krate(db));
                        let requests = compiled.as_ref().ok().map(|it| &it.requests);
                        (
                            Node::Instance(*instance),
                            krate,
                            body_files(db, instance),
                            requests,
                        )
                    }
                };
                stack.extend(requests.map(|it| (it, by)));
                let walked = requests.is_some();
                reached.push(Reached { node, env: *env, by, krate, files, walked });
            }
        }
        reached
    }

    fn mark(&self, db: &Db) -> Option<Mark> {
        let (serial, compiled) = match &self.node {
            Node::Root(func) => (root_serial(db, *func), compiled_root(db, *func)),
            Node::Instance(instance) => (
                instance_serial(db, *instance, self.env),
                compiled_instance(db, *instance, self.env),
            ),
            Node::Static(_) => return None,
        };
        Some(Mark::new(db, serial, &compiled.as_ref().ok()?.requests))
    }

    /// The function whose code it is.
    fn func(&self, db: &Db) -> Option<FunctionId> {
        match &self.node {
            Node::Root(func) => Some(*func),
            Node::Instance(instance) => written_in(db, instance),
            Node::Static(_) => None,
        }
    }
}

#[derive(Clone)]
pub(super) struct BaseStatic {
    pub(super) symbol: String,
    pub(super) writable: bool,
    mark: Mark,
    krate: Crate,
    /// The file its value is written in.
    file: FileId,
    parents: Vec<Node>,
    /// The functions left to rustc reach it.
    theirs: bool,
    /// rustc's copy, which has state that rustc's code shares with ours: a patch can't give
    /// it another first value.
    pub(super) shared: bool,
    /// The size and alignment of its value, `None` for one that did not compile: a value
    /// larger or aligned to more does not fit where the base has it.
    layout: Option<(u64, u64)>,
}

/// What a function left to rustc keeps the rest of the build from doing.
#[derive(Clone, PartialEq, Eq)]
struct Left {
    /// Its code, when the function compiles and something else keeps it from the object.
    /// A full build looks at it again once that changes.
    code: Option<Mark>,
    async_blocks: Option<FxHashSet<salsa::Id>>,
}

impl Left {
    fn of(db: &Db, root: FunctionId) -> Left {
        let code = compiled_root(db, root).as_ref().ok();
        Left {
            code: code.map(|it| Mark::new(db, root_serial(db, root), &it.requests)),
            // clone: kept, once per root when the base or the daemon first notes it
            async_blocks: async_blocks(db, root).clone(),
        }
    }

    /// Whether [`Left::of`] `root` is `self` now.
    fn still(&self, db: &Db, root: FunctionId) -> bool {
        let code = compiled_root(db, root).as_ref().ok();
        let code = code.map(|it| Mark::new(db, root_serial(db, root), &it.requests));
        let async_blocks = async_blocks(db, root);
        code == self.code && *async_blocks == self.async_blocks
    }
}

/// A function left to rustc, or an instance our code calls rustc's copy of, whose copy is no
/// longer right for it.
pub struct OutOfDate {
    /// `None` for an instance written in no function, as drop glue is.
    pub func: Option<FunctionId>,
    /// Why blaze leaves it to rustc.
    pub why: String,
    /// Whether it was left to rustc in the build the daemon started from, when rustc compiled
    /// it from source that built: the reason is then blaze's, not an error of the source.
    pub left_from_the_start: bool,
    /// What rustc's copy calls a copy of rustc's of that changed, when it is not the function
    /// itself. `None` in it for code of no function of its own.
    pub calls: Option<Option<FunctionId>>,
}

/// The functions left to rustc as rustc compiled them, and what of rustc's they call: its
/// copies are not built again while the daemon runs, so one is only good for as long as its
/// function, and what it calls of rustc's, is what it was then.
#[derive(Clone, Default)]
pub(crate) struct Frozen {
    /// `None` for one an edit had reached by the time it was first left to rustc: what rustc
    /// compiled of it is not known.
    left: Option<FxHashMap<FunctionId, Option<Left>>>,
    /// What each of [`Reached`] compiled to when first reached, with `None` as for `left`, and
    /// `Some(None)` for one that did not compile.
    reached: FxHashMap<(Node, StoredParamEnvAndCrate), Option<Option<Mark>>>,
    /// The crates that have seen an edit since.
    edited: FxHashSet<Crate>,
    /// Those of `edited` whose first values of statics an edit can have changed: of a change
    /// to an included file, only the crates that include it and what depends on them
    /// ([`Frozen::edit_apart`]).
    edited_values: FxHashSet<Crate>,
    /// The files that have seen an edit to the bodies of functions alone, which can change
    /// no function written elsewhere.
    edited_bodies: FxHashSet<FileId>,
    /// The crates rustc compiled from other source than the daemon first saw them with: no
    /// copy of a function of theirs is good.
    distrusted: FxHashSet<Crate>,
    /// The first value rustc gave each static with state that a build asked about
    /// ([`RustcValues`]).
    statics: Arc<std::sync::Mutex<FxHashMap<StaticId, Option<Mark>>>>,
    /// The crates whose statics [`Frozen::take_values`] took.
    taken: FxHashSet<Crate>,
}

/// The first value rustc gave each static with state (a thread-local, or one written at run
/// time), as rustc compiled it: such a static stays rustc's copy only while its first value
/// is still that one (`docs/live.md#statics`). Taken the first time a build asks, or before
/// the first edit of its crate or one it depends on ([`Frozen::take_values`]), when no edit
/// since the daemon started can have reached the static; `None` when one had, and rustc's
/// value is not known.
#[derive(Clone, Default)]
pub(crate) struct RustcValues {
    known: Arc<std::sync::Mutex<FxHashMap<StaticId, Option<Mark>>>>,
    /// The crates an edit since the start can have changed a value of: those edited and
    /// what depends on them, and those rustc compiled from other source.
    touched: Arc<FxHashSet<Crate>>,
}

impl RustcValues {
    /// Whether rustc's copy of `id` still starts from the value the source gives it now. One
    /// that does not compile has nothing of ours to tell by, and is rustc's.
    ///
    /// An edit of bodies alone is no edit of a value: those are bodies of functions that are
    /// not `const` ([`Rejections::touch_bodies`]), which no value is evaluated through.
    pub(crate) fn kept(&self, db: &Db, id: StaticId) -> bool {
        let Ok(compiled) = compiled_static(db, id).as_ref() else { return true };
        let now = Mark::new(db, static_serial(db, id), &compiled.requests);
        let touched = self.touched.contains(&id.krate(db));
        let mut known = self.known.lock().unwrap_or_else(|e| e.into_inner());
        let was = *known.entry(id).or_insert((!touched).then_some(now));
        was == Some(now)
    }
}

impl Frozen {
    /// What a build asks of [`RustcValues`]: the values taken so far, shared with it, and the
    /// edits as they are now.
    pub(crate) fn rustc_values(&self) -> RustcValues {
        // alloc: once per build, the edits as the build sees them
        let touched = self.edited_values.iter().chain(&self.distrusted).copied().collect();
        // clone: once per build, the shared table the build takes values into
        RustcValues { known: self.statics.clone(), touched: Arc::new(touched) }
    }

    pub(crate) fn distrust(&mut self, crates: impl IntoIterator<Item = Crate>) {
        self.distrusted.extend(crates);
    }

    pub(crate) fn edit_bodies(&mut self, file: FileId) {
        self.edited_bodies.insert(file);
    }

    pub(crate) fn edit(&mut self, crates: impl IntoIterator<Item = Crate>) {
        for krate in crates {
            self.edited.insert(krate);
            self.edited_values.insert(krate);
        }
    }

    /// [`Frozen::edit`] of `crates`, where the first values of statics an edit can have
    /// changed are only those of `values`.
    pub(crate) fn edit_apart(
        &mut self,
        crates: impl IntoIterator<Item = Crate>,
        values: impl IntoIterator<Item = Crate>,
    ) {
        self.edited.extend(crates);
        self.edited_values.extend(values);
    }

    /// Takes the first value of each static with state that `crates` declare, of those no
    /// edit has reached: asked before an edit reaches them, so that a build that first asks
    /// about one after it, as a build in full of code no build compiled before does, still
    /// knows rustc's ([`RustcValues`]).
    pub(crate) fn take_values(&mut self, db: &Db, crates: FxHashSet<Crate>) {
        let untouched =
            |it: &Crate| !self.edited_values.contains(it) && !self.distrusted.contains(it);
        let fresh: Vec<Crate> =
            // alloc: once per build, the crates no build took the values of before
            crates.into_iter().filter(|it| untouched(it) && !self.taken.contains(it)).collect();
        if fresh.is_empty() {
            return;
        }
        self.taken.extend(fresh.iter().copied());
        let statics = super::stable::declared_statics(db, &fresh);
        let mut known = self.statics.lock().unwrap_or_else(|e| e.into_inner());
        for id in statics {
            if known.contains_key(&id) {
                continue;
            }
            let Ok(compiled) = compiled_static(db, id).as_ref() else { continue };
            if is_thread_local(db, id) || compiled.writable {
                let mark = Mark::new(db, static_serial(db, id), &compiled.requests);
                known.insert(id, Some(mark));
            }
        }
    }

    /// Whether an edit since the daemon started can have changed what the root is.
    fn touched_root(&self, db: &Db, root: FunctionId) -> bool {
        self.edited.contains(&root.krate(db))
            || self.edited_bodies.contains(&body_file(db, root.into()))
    }

    /// Whether an edit since the daemon started can have changed what rustc compiled of a
    /// function of `krate` that is generic over nothing: an edit of the crate, or of what it
    /// depends on, which is in `edited` too; or of the bodies of a file of the crate or of one
    /// it depends on, whose generic and inline functions rustc copies into it.
    fn touched_from(&self, db: &Db, krate: Crate) -> bool {
        if self.edited.contains(&krate) {
            return true;
        }
        if self.edited_bodies.is_empty() {
            return false;
        }
        let deps = krate.transitive_deps(db);
        self.edited_bodies
            .iter()
            .any(|&file| base_db::relevant_crates(db, file).iter().any(|it| deps.contains(it)))
    }

    fn touched(&self, db: &Db, reached: &Reached) -> bool {
        let bodies = match &reached.files {
            // What it calls is of its crates or of what they depend on: rustc copies the
            // generic functions of those into it.
            _ if !reached.walked => {
                let own = reached.krate.transitive_deps(db);
                let env = reached.env.krate.transitive_deps(db);
                let in_crates = |file: FileId| {
                    let relevant = base_db::relevant_crates(db, file);
                    relevant.iter().any(|it| own.contains(it) || env.contains(it))
                };
                self.edited_bodies.iter().any(|&it| in_crates(it))
            }
            Some(files) => files.iter().any(|it| self.edited_bodies.contains(it)),
            None => !self.edited_bodies.is_empty(),
        };
        bodies || self.edited.contains(&reached.krate) || self.edited.contains(&reached.env.krate)
    }

    /// A function or instance of `base` that rustc's copy is no longer right for, and why. The
    /// first call with a base of ours notes what rustc compiled: of what no edit has reached by
    /// then.
    pub(crate) fn out_of_date(&mut self, db: &Db, base: &Base) -> Option<OutOfDate> {
        let now = || {
            let theirs = base.skipped.iter().filter(|it| !it.1.no_symbol());
            theirs.map(|(root, why)| (*root, why))
        };
        let distrusted = |root: FunctionId| self.distrusted.contains(&root.krate(db));
        let Some(left) = &self.left else {
            let other = now().find(|it| distrusted(it.0)).map(|(func, why)| OutOfDate {
                func: Some(func),
                // alloc: the reason a full build is told, once it is out of date
                why: why.to_string(),
                left_from_the_start: true,
                calls: None,
            });
            let left = now()
                .map(|(root, _)| (root, (!self.touched_root(db, root)).then(|| Left::of(db, root))))
                // alloc: once per daemon, what rustc compiled at the start
                .collect();
            self.left = Some(left);
            return other.or_else(|| self.out_of_date(db, base));
        };
        let own = now().find_map(|(root, why)| {
            let fresh = match left.get(&root) {
                _ if distrusted(root) => false,
                // Nothing to tell a change by: any edit that it can see may be one.
                Some(Some(Left { code: None, .. })) => !self.touched_root(db, root),
                Some(Some(was)) => probe(|| was.still(db, root)) == Some(true),
                Some(None) => false,
                // Not left at first, as one compiled then: rustc's copy is still of the source
                // the daemon started with, when no edit since reaches it.
                None => !self.touched_from(db, root.krate(db)),
            };
            (!fresh).then(|| OutOfDate {
                func: Some(root),
                // alloc: the reason a full build is told, once it is out of date
                why: why.to_string(),
                left_from_the_start: left.contains_key(&root),
                calls: None,
            })
        });
        if own.is_some() {
            return own;
        }
        // alloc: called only for the reason a full build is told, once it is out of date
        let why = |by| base.skipped.iter().find(|it| it.0 == by).map(|it| it.1.to_string());
        for reached in &base.reached {
            let key = (reached.node, reached.env);
            let touched = self.touched(db, reached);
            let fresh = match self.reached.get(&key) {
                Some(Some(_)) if !touched => true,
                Some(Some(Some(was))) => probe(|| reached.mark(db)).flatten() == Some(*was),
                // Without code to tell a change by, any edit that reaches it may be one.
                Some(Some(None) | None) => false,
                None => {
                    let known = (!touched).then(|| probe(|| reached.mark(db)).flatten());
                    self.reached.insert(key, known);
                    !touched
                }
            };
            if !fresh {
                return Some(OutOfDate {
                    func: Some(reached.by),
                    why: why(reached.by).unwrap_or_default(),
                    left_from_the_start: true,
                    calls: Some(reached.func(db).filter(|&it| it != reached.by)),
                });
            }
        }
        // Our code calls rustc's copy of an instance that did not compile, which is of its
        // function as rustc compiled it.
        let edited = !self.edited.is_empty() || !self.edited_bodies.is_empty();
        let mut forwarded = base.instances.iter().filter(|it| edited && it.1.forwarded);
        forwarded.find_map(|(it, known)| {
            let (crates, files) = (mentioned_crates(db, it), known.files.as_deref());
            let untouched = match (crates, files) {
                (Some(crates), Some(files)) => {
                    crates.is_disjoint(&self.edited)
                        && files.iter().all(|it| !self.edited_bodies.contains(it))
                }
                // Of a function generic over nothing, rustc's copy has only what its crate
                // and those below it wrote.
                _ if it.args.as_ref().iter().all(|arg| arg.region().is_some()) => {
                    written_in(db, it).is_some_and(|func| !self.touched_from(db, func.krate(db)))
                }
                _ => false,
            };
            (!untouched).then(|| OutOfDate {
                func: written_in(db, it),
                // alloc: the reason a full build is told, once it is out of date
                why: "an instance of it does not compile".to_owned(),
                left_from_the_start: false,
                calls: None,
            })
        })
    }
}

/// The last object built in full, as far as patching it goes.
#[derive(Clone)]
pub(super) struct Base {
    /// Whether the object says which lines its code is of, as a patch of it then does.
    debug: bool,
    entries: Vec<FunctionId>,
    pub(super) compiled: Vec<(FunctionId, String)>,
    marks: Vec<Mark>,
    /// The crate of each of `compiled`, and the file it is written in.
    crates: Vec<Crate>,
    files: Vec<FileId>,
    touched: Touched,
    pub(super) skipped: Vec<(FunctionId, CodegenError)>,
    /// For each of `skipped` that rustc has a copy of.
    left: Vec<Option<Left>>,
    /// The skipped functions that rustc has a copy of.
    from_rustc: Vec<FunctionId>,
    their_statics: TheirStatics,
    their_walk: TheirWalk,
    /// What rustc's copies of `from_rustc` call of rustc's.
    reached: Vec<Reached>,
    pub(super) instances: FxIndexMap<Instance, BaseInstance>,
    homes: Homes,
    pub(super) statics: FxHashMap<StaticId, BaseStatic>,
    outdated: bool,
    /// Why what rustc linked can't be patched any longer, when that is what this is.
    outdated_by: Option<&'static str>,
    /// The instances that did not compile when the base was built. What called them was left
    /// to rustc, and stays there for as long as the base does.
    rejected: Vec<(Instance, StoredParamEnvAndCrate)>,
    pub(super) bodies: usize,
    /// The roots and instances that are `#[blaze::cache]` functions, by their index in
    /// `compiled` and `instances`. The hash of what each reaches is in its code, and an edit
    /// anywhere can change that.
    cached_roots: Vec<u32>,
    cached_instances: Vec<u32>,
    /// Whether the base is the executable rustc linked, and not an object of ours: every
    /// function and static in it is rustc's. It knows the files that were about to change,
    /// in `homed`, as they were when rustc compiled them, and nothing else.
    pub(super) rustc: bool,
    homed: FxHashSet<FileId>,
    /// The functions of the `homed` files that rustc's symbols name no copy of, with what
    /// each compiled to then: the symbols may miss one rustc has, whose callers no patch
    /// reaches, so one that changes declines.
    unnamed: Vec<(FunctionId, Option<Mark>, FileId)>,
    /// The closures written in the functions of the `homed` files.
    closures: Vec<HomedClosures>,
    /// The generic functions of the modules of each `homed` file by name, and whether each of
    /// rustc's copies of those of the name is an instance of the base, which a patch replaces
    /// under rustc's symbol.
    copies_homed: FxHashMap<FileId, FxHashMap<String, bool>>,
}

/// The closures rustc compiled of the body of a function of a file an edit changes. rustc's
/// code that takes one as an argument is of its type then: one that captures or is called
/// otherwise declines, as does one rustc's symbols name no copy of that changed.
#[derive(Clone)]
struct HomedClosures {
    root: FunctionId,
    file: FileId,
    /// As [`super::closures_of`] has them.
    closures: Vec<Option<(Instance, u64)>>,
    /// Those rustc's symbols name no copy of, or that did not compile, with what each
    /// compiled to then. The others are among the instances of the base.
    unnamed: Vec<(Instance, Option<Mark>)>,
    /// [`super::written_print`] of the root, for one with an `async` block, which no mark
    /// here says a change to.
    written: Option<u64>,
}

/// Where in the base the code of each crate is, and the code that comes from the bodies of
/// each file: an edit is looked for there, which is far less than the base.
#[derive(Clone, Default)]
struct Homes {
    root: FxHashMap<FunctionId, u32>,
    roots_of_crate: FxHashMap<Crate, Vec<u32>>,
    roots_of_file: FxHashMap<FileId, Vec<u32>>,
    instances_of_crate: FxHashMap<Crate, Vec<u32>>,
    instances_of_file: FxHashMap<FileId, Vec<u32>>,
    /// The instances that the bodies of any file may be part of.
    instances_of_any_file: Vec<u32>,
}

impl Homes {
    fn new(
        compiled: &[(FunctionId, String)],
        crates: &[Crate],
        files: &[FileId],
        instances: &FxIndexMap<Instance, BaseInstance>,
    ) -> Homes {
        let mut homes = Homes::default();
        for (i, &(root, _)) in compiled.iter().enumerate() {
            homes.root.insert(root, i as u32);
            homes.roots_of_crate.entry(crates[i]).or_default().push(i as u32);
            homes.roots_of_file.entry(files[i]).or_default().push(i as u32);
        }
        for (i, was) in instances.values().enumerate() {
            homes.instances_of_crate.entry(was.env.krate).or_default().push(i as u32);
            match &was.files {
                Some(files) => {
                    for &file in files {
                        homes.instances_of_file.entry(file).or_default().push(i as u32);
                    }
                }
                None => homes.instances_of_any_file.push(i as u32),
            }
        }
        homes
    }
}

/// One part in `parts` of `all`, in order and each once.
fn part_of(mut all: Vec<u32>, part: usize, parts: usize) -> impl Iterator<Item = usize> {
    blaze_ids::radix::sort(&mut all);
    all.dedup();
    all.into_iter().skip(part).step_by(parts).map(|it| it as usize)
}

/// What an edit since the base was last looked at can have changed the code of, and what
/// differed from the base then, which does until an edit changes it back. Code compiled as a
/// crate depends on nothing but the files of that crate and of the ones it depends on. Of a
/// file where only the bodies of functions changed, and none of them is one that the rest of
/// the program sees into, it depends on no more than the bodies it comes from.
#[derive(Clone, Default)]
struct Touched {
    /// The crates that see a file that changed.
    crates: FxHashSet<Crate>,
    /// The files that changed in the bodies of functions alone.
    bodies: FxHashSet<FileId>,
    /// A file changed that we can't tell the crates of.
    any: bool,
    /// What the last look found changed.
    roots: FxHashSet<FunctionId>,
    instances: FxHashSet<Instance>,
    statics: FxHashSet<StaticId>,
}

impl Touched {
    fn reaches(&self, krate: Crate) -> bool {
        self.any || self.crates.contains(&krate)
    }

    /// Whether a file changed since the last look.
    fn edited(&self) -> bool {
        self.any || !self.crates.is_empty() || !self.bodies.is_empty()
    }

    /// Starts over from what a look at the whole base found: the edits before it can have
    /// changed nothing else.
    fn settle(&mut self, found: &[Stale]) {
        // A look that gave up, or that a thread did not come back from, did not get to
        // everything.
        if found.is_empty() || found.iter().any(|it| it.declined.is_some()) {
            return;
        }
        *self = Touched::default();
        for stale in found {
            self.roots.extend(&stale.roots);
            self.instances.extend(&stale.instances);
            self.statics.extend(&stale.statics);
            for (node, _) in &stale.broken {
                match node {
                    Node::Instance(instance) => _ = self.instances.insert(*instance),
                    Node::Static(id) => _ = self.statics.insert(*id),
                    Node::Root(_) => {}
                }
            }
        }
    }

    fn reaches_bodies(&self, files: Option<&[FileId]>) -> bool {
        if self.bodies.is_empty() {
            return false;
        }
        files.is_none_or(|files| files.iter().any(|it| self.bodies.contains(it)))
    }
}

/// What a look at a part of the base found changed.
#[derive(Clone, Default)]
pub struct Stale {
    roots: Vec<FunctionId>,
    instances: Vec<Instance>,
    /// The statics with another value, which a patch defines again under their name.
    statics: Vec<StaticId>,
    /// What a patch can't replace, and why. That only matters for what the program still
    /// reaches.
    broken: Vec<(Node, &'static str)>,
    /// The functions of the base an edit took away: what rustc or the base has of them stays
    /// where nothing calls it any more.
    gone: Vec<FunctionId>,
    /// Why no patch can hold the change.
    declined: Option<&'static str>,
    /// Something the functions left to rustc reach changed, so what they reach may have.
    their_walk_changed: bool,
    pub steps: StaleSteps,
}

/// How long looking at each kind of thing in the base took, in the order they are looked at.
#[derive(Default, Clone, Copy)]
pub struct StaleSteps {
    pub roots: std::time::Duration,
    pub instances: std::time::Duration,
    pub statics: std::time::Duration,
    /// The functions left to rustc.
    pub left: std::time::Duration,
    /// How many roots and instances an edit could have changed, and so were asked about.
    pub roots_asked: usize,
    pub instances_asked: usize,
    /// The longest any one of them took to answer.
    pub slowest: std::time::Duration,
}

impl Stale {
    /// Declines when the functions the base left to rustc reach other statics than they did.
    fn declined(mut self, why: &'static str) -> Stale {
        self.declined = Some(why);
        self
    }
}

/// What of the base an edit left as it was: everything but these.
#[derive(Default)]
pub(super) struct Unchanged {
    pub(super) changed: FxHashSet<Node>,
    pub(super) broken: FxHashSet<Node>,
    /// The roots the edit took away, which refer to nothing any more.
    pub(super) gone: FxHashSet<FunctionId>,
    /// What changed (or is broken) that nothing the edit left reaches any more, by each
    /// unchanged instance or static of the base whose code still refers to it: the patch
    /// compiles none of it, and code that refers to one of those again would reach the base's
    /// copy, which is of the source before.
    pub(super) behind: FxHashMap<Node, Vec<Node>>,
    /// The statics of `changed` whose value no longer fits where the base has them: each is
    /// a static of the patch's own, and everything of the base that refers to it is in the
    /// patch too, referring to that ([`Base::moved`]).
    pub(super) moved: FxHashSet<StaticId>,
}

pub(super) struct Patch {
    pub(super) object: Vec<u8>,
    pub(super) bodies: usize,
    pub(super) phases: Phases,
}

/// An object that defines nothing.
pub(super) fn empty_object() -> Result<Vec<u8>> {
    let builder =
        // clone: once per object, the builder owns its target
        ObjectBuilder::new(target()?.clone(), "blaze", cranelift_module::default_libcall_names())?;
    let object = ObjectModule::new(builder)
        .finish()
        .emit()
        // alloc: the error's message
        .map_err(|e| CodegenError::Backend(e.to_string()))?;
    if super::Target::current().writes_wasm_objects() {
        return super::wasm_object::from_elf(&object, &Default::default(), &[])
            .map_err(CodegenError::Backend);
    }
    Ok(object)
}

impl Base {
    /// `None` when the build has something in it a patch could not keep track of.
    pub(super) fn record(
        db: &Db,
        cg: &ObjectCodegen<'_, '_>,
        entries: &[FunctionId],
        compiled: &[(FunctionId, String)],
        skipped: &[(FunctionId, CodegenError)],
    ) -> Option<Base> {
        let decls = cg.module.declarations();
        let debug = cg.debug.is_some();
        let root_mark = |db: &Db, root| Mark::of_root(db, root, debug);
        let instance_mark =
            |db: &Db, (instance, env): &(Instance, StoredParamEnvAndCrate)| {
                (Mark::of_instance(db, instance, env, debug), body_files(db, instance))
            };
        // The marks are a hash of each function's code and of its lines, which every thread
        // works out: the object waits for this.
        let (envs, roots) = (cg.envs.as_slice(), compiled.len());
        // alloc: once per full build, a slot per root and instance for the threads to fill
        let root_marks: Vec<_> = (0..roots).map(|_| std::sync::OnceLock::new()).collect();
        // alloc: once per full build, as `root_marks`
        let instance_marks: Vec<_> = envs.iter().map(|_| std::sync::OnceLock::new()).collect();
        cg.symbols.in_parallel(roots + envs.len(), &|db, _, at| match at.checked_sub(roots) {
            Some(at) => _ = instance_marks[at].set(instance_mark(db, &envs[at])),
            None => _ = root_marks[at].set(root_mark(db, compiled[at].0)),
        });
        let marks = compiled
            .iter()
            .zip(root_marks)
            .map(|(&(root, _), mark)| mark.into_inner().unwrap_or_else(|| root_mark(db, root)))
            // alloc: once per full build, kept in the base
            .collect::<Option<Vec<_>>>()?;
        let mut instances = envs
            .iter()
            .zip(instance_marks)
            .map(|(of, mark)| {
                let (instance, env) = of;
                let id = *cg.instances.get(instance)?;
                let (mark, files) = mark.into_inner().unwrap_or_else(|| instance_mark(db, of));
                let known = BaseInstance {
                    env: *env,
                    symbol: decls.get_function_decl(id).linkage_name(id).into_owned(),
                    mark,
                    forwarded: cg.forwarded.contains(instance),
                    files,
                    // alloc: empty, allocates nothing until a parent is noted
                    parents: Vec::new(),
                    theirs: false,
                };
                (known.mark.is_some() || known.forwarded).then(|| (*instance, known))
            })
            // alloc: once per full build, kept in the base
            .collect::<Option<FxIndexMap<_, _>>>()?;
        // One that does not compile is never what it was: a change to the file declines.
        let nothing = Mark { serial: 0, locations: 0, lines: 0 };
        let base_static =
            |id: StaticId, data, compiled: Option<&CompiledStatic>, shared| BaseStatic {
                symbol: decls.get_data_decl(data).linkage_name(data).into_owned(),
                writable: compiled.is_some_and(|it| it.writable),
                mark: compiled
                    .map_or(nothing, |it| Mark::new(db, static_serial(db, id), &it.requests)),
                krate: id.krate(db),
                file: body_file(db, id.into()),
                // alloc: empty, allocates nothing until a parent is noted
                parents: Vec::new(),
                theirs: false,
                shared,
                layout: compiled.map(CompiledStatic::layout),
            };
        let mut statics = cg
            .statics
            .iter()
            .chain(&cg.frozen_statics)
            .map(|(&id, &data)| {
                let compiled = compiled_static(db, id).as_ref().ok()?;
                Some((id, base_static(id, data, Some(compiled), false)))
            })
            // alloc: once per full build, kept in the base
            .collect::<Option<FxHashMap<_, _>>>()?;
        // rustc's copies with state, so that an edit to a first value is seen.
        for (&id, &data) in &cg.shared_statics {
            let compiled = compiled_static(db, id).as_ref().ok();
            statics.insert(id, base_static(id, data, compiled, true));
        }
        // alloc: once per full build, what refers to what in it
        let mut edges = Vec::new();
        let mut note = |parent: Node, requests: &Requests| {
            for request in &requests.funcs {
                if let FuncTarget::Instance(instance, _) = &request.target {
                    edges.push((Node::Instance(*instance), parent));
                }
            }
            for request in &requests.datas {
                if let DataRequest::Static(id) = request {
                    edges.push((Node::Static(*id), parent));
                }
            }
        };
        for &(root, _) in compiled {
            note(Node::Root(root), &compiled_root(db, root).as_ref().ok()?.requests);
        }
        for (instance, known) in &instances {
            if let Ok(compiled) = compiled_instance(db, *instance, known.env) {
                note(Node::Instance(*instance), &compiled.requests);
            }
        }
        for &id in statics.keys() {
            note(Node::Static(id), &compiled_static(db, id).as_ref().ok()?.requests);
        }
        for (child, parent) in edges {
            let parents = match &child {
                Node::Root(_) => None,
                Node::Instance(instance) => instances.get_mut(instance).map(|it| &mut it.parents),
                Node::Static(id) => statics.get_mut(id).map(|it| &mut it.parents),
            };
            if let Some(parents) = parents {
                parents.push(parent);
            }
        }
        let left =
            // alloc: once per full build, kept in the base
            skipped.iter().map(|(root, e)| (!e.no_symbol()).then(|| Left::of(db, *root))).collect();
        // alloc: once per full build, kept in the base
        let from_rustc: Vec<FunctionId> = left_to_rustc(db, cg.symbols, skipped).collect();
        let (their_statics, walked) = TheirStatics::walk(db, from_rustc.iter().copied());
        let their_walk = TheirWalk::new(db, &their_statics, walked, &mut instances, &mut statics);
        let reached = Reached::walk(db, &from_rustc, compiled, skipped);
        let rejected = cg
            .rejected
            .iter()
            .filter(|(instance, it)| compiled_instance(db, **instance, it.env).is_err())
            .map(|(instance, it)| (*instance, it.env))
            // alloc: once per full build, kept in the base
            .collect();
        // alloc: once per full build, kept in the base
        let crates: Vec<Crate> = compiled.iter().map(|&(root, _)| root.krate(db)).collect();
        let files: Vec<FileId> =
            // alloc: once per full build, kept in the base
            compiled.iter().map(|&(root, _)| body_file(db, root.into())).collect();
        let cached = |func| super::fncache::cached_closure(db, func).is_some();
        let cached_roots =
            // alloc: once per full build, kept in the base
            (0..compiled.len() as u32).filter(|&i| cached(compiled[i as usize].0)).collect();
        let cached_instances = (0..instances.len() as u32)
            .filter(|&i| {
                let (instance, _) = instances.get_index(i as usize).unwrap();
                matches!(instance.def, InstanceDef::Fn(func) if cached(func))
            })
            // alloc: once per full build, kept in the base
            .collect();
        Some(Base {
            debug,
            rejected,
            // alloc: once per full build, kept in the base
            entries: entries.to_vec(),
            // alloc: once per full build, kept in the base
            compiled: compiled.to_vec(),
            marks,
            homes: Homes::new(compiled, &crates, &files, &instances),
            crates,
            files,
            touched: Touched::default(),
            // alloc: once per full build, kept in the base
            skipped: skipped.to_vec(),
            left,
            from_rustc,
            their_statics,
            their_walk,
            reached,
            instances,
            statics,
            outdated: false,
            outdated_by: None,
            bodies: cg.bodies,
            cached_roots,
            cached_instances,
            rustc: false,
            homed: FxHashSet::default(),
            // alloc: empty, allocates nothing
            unnamed: Vec::new(),
            // alloc: empty, allocates nothing
            closures: Vec::new(),
            copies_homed: FxHashMap::default(),
        })
    }

    /// Takes in what rustc compiled of `file` before an edit changes it: the `functions`
    /// written in it that rustc has a symbol for, and the `statics` declared in it or in their
    /// bodies. The source is what rustc compiled until then, so the marks are of rustc's copies.
    pub(super) fn home(
        &mut self,
        db: &Db,
        // dyn: the build's one resolver, made by blaze-engine or blaze-wasm (which depend on this crate)
        symbols: &dyn SymbolResolver,
        file: FileId,
        functions: &[FunctionId],
        statics: &[StaticId],
        callers: &[FunctionId],
    ) {
        if !self.homed.insert(file) {
            return;
        }
        for &root in functions {
            let known = self.homes.root.contains_key(&root)
                || self.skipped.iter().any(|it| it.0 == root)
                || self.unnamed.iter().any(|it| it.0 == root);
            // rustc copies a generic one into what calls it: a change to one is a change to
            // the file's copies, which no patch of what rustc linked is made for.
            if known || copied_by_rustc(db, root) {
                continue;
            }
            self.home_closures(db, symbols, root);
            let Some(symbol) = symbols.function(root) else {
                let mark = Mark::of_root(db, root, self.debug);
                self.unnamed.push((root, mark, body_file(db, root.into())));
                continue;
            };
            match compiled_root(db, root) {
                Ok(_) => {
                    let mark = Mark::of_root(db, root, self.debug);
                    self.marks.push(mark.expect("a root that compiles has a mark"));
                    // alloc: the patch's base keeps it
                    self.compiled.push((root, symbol.as_str().to_owned()));
                    self.crates.push(root.krate(db));
                    self.files.push(body_file(db, root.into()));
                }
                Err(e) => {
                    // clone: the error, kept in the base
                    self.skipped.push((root, e.clone()));
                    self.left.push(Some(Left::of(db, root)));
                }
            }
        }
        for &id in statics {
            if self.statics.contains_key(&id) {
                continue;
            }
            let compiled = compiled_static(db, id).as_ref().ok();
            // One that does not compile is never what it was: a change to the file declines.
            let mark = compiled.map_or(Mark { serial: 0, locations: 0, lines: 0 }, |it| {
                Mark::new(db, static_serial(db, id), &it.requests)
            });
            let known = BaseStatic {
                // alloc: the patch's base keeps it
                symbol: symbols.static_(id).map_or_else(String::new, |it| it.as_str().to_owned()),
                writable: compiled.is_some_and(|it| it.writable),
                mark,
                krate: id.krate(db),
                file: body_file(db, id.into()),
                // alloc: empty, allocates nothing until a parent is noted
                parents: Vec::new(),
                theirs: false,
                // rustc's copy, which rustc's code writes when the static has state.
                shared: is_thread_local(db, id) || compiled.is_some_and(|it| it.writable),
                layout: compiled.map(CompiledStatic::layout),
            };
            self.statics.insert(id, known);
        }
        let homed = self.home_copies(db, symbols, functions, callers);
        self.copies_homed.insert(file, homed);
        self.homes = Homes::new(&self.compiled, &self.crates, &self.files, &self.instances);
    }

    /// Whether [`Self::home`] noted `file`.
    pub(super) fn homed(&self, file: FileId) -> bool {
        self.homed.contains(&file)
    }

    /// Whether [`Self::home`] took in each of rustc's copies of the generic functions of the
    /// modules of `file` named `names`.
    pub(super) fn copies_homed(&self, file: FileId, names: &[String]) -> bool {
        let Some(homed) = self.copies_homed.get(&file) else { return false };
        names.iter().all(|it| homed.get(it).copied().unwrap_or(false))
    }

    /// Takes in rustc's copies of the generic functions of a file's modules, `functions` being
    /// what is written in it: the instances its other functions and `callers` reach, through
    /// the generic ones of `callers` too, and those reach in turn, that rustc's executable
    /// has, and the rest whose arguments rustc's path of them spells in types read back. A
    /// patch replaces one under rustc's symbol, as it does a closure. Whether those are all
    /// the copies the executable has of the functions of each name.
    fn home_copies(
        &mut self,
        db: &Db,
        // dyn: the build's one resolver, made by blaze-engine or blaze-wasm (which depend on this crate)
        symbols: &dyn SymbolResolver,
        functions: &[FunctionId],
        callers: &[FunctionId],
    ) -> FxHashMap<String, bool> {
        // Of a module or an `impl` that is not generic, as the outline of a file tells them.
        let of_modules = |func: FunctionId| match func.lookup(db).container {
            hir_def::ItemContainerId::ModuleId(_) => true,
            hir_def::ItemContainerId::ImplId(imp) => {
                let generics = hir_ty::generics::generics(db, imp.into());
                !generics
                    .iter(false)
                    .any(|(id, _)| !matches!(id, hir_def::GenericParamId::LifetimeParamId(_)))
            }
            _ => false,
        };
        let generic: FxHashSet<FunctionId> = functions
            .iter()
            .copied()
            .filter(|&it| copied_by_rustc(db, it) && of_modules(it))
            // alloc: once per file the base takes in
            .collect();
        let mut homed: FxHashMap<String, bool> = FxHashMap::default();
        if generic.is_empty() {
            return homed;
        }
        // rustc's copies by their paths, with the function each is of.
        let mut copies: FxHashMap<String, (FunctionId, String)> = FxHashMap::default();
        let mut unknown: FxHashSet<FunctionId> = FxHashSet::default();
        for &func in &generic {
            let Some(found) = symbols.rustc_instances(func) else {
                unknown.insert(func);
                continue;
            };
            for (path, symbol) in found {
                if let Some((other, _)) = copies.insert(path, (func, symbol)) {
                    unknown.extend([func, other]);
                }
            }
        }
        let through: FxHashSet<FunctionId> = generic
            .iter()
            .copied()
            .chain(callers.iter().copied().filter(|&it| copied_by_rustc(db, it)))
            // alloc: once per file the base takes in
            .collect();
        let note = |requests: &Requests, queue: &mut Vec<(Instance, StoredParamEnvAndCrate)>| {
            for request in &requests.funcs {
                if let FuncTarget::Instance(instance, env) = &request.target
                    && let InstanceDef::Fn(func) = instance.def
                    && through.contains(&func)
                {
                    queue.push((*instance, *env));
                }
            }
        };
        // alloc: once per file the base takes in, the walk's queue
        let mut queue = Vec::new();
        let roots = functions.iter().chain(callers).copied().filter(|&it| !copied_by_rustc(db, it));
        for root in roots {
            let Ok(compiled) = compiled_root(db, root) else { continue };
            note(&compiled.requests, &mut queue);
            let env = closure_env(db, root);
            for (_, closure) in super::closures_of(db, root).into_iter().flatten() {
                let Some((instance, _)) = closure else { continue };
                let compiled = compiled_instance(db, instance, env);
                if let Ok(compiled) = compiled.as_ref() {
                    note(&compiled.requests, &mut queue);
                }
            }
        }
        let mut found: FxIndexMap<Instance, StoredParamEnvAndCrate> = FxIndexMap::default();
        while let Some((instance, env)) = queue.pop() {
            if found.contains_key(&instance) {
                continue;
            }
            if let Ok(compiled) = compiled_instance(db, instance, env).as_ref() {
                note(&compiled.requests, &mut queue);
            }
            found.insert(instance, env);
        }
        let spelling = super::Spelling { db, symbols };
        // alloc: once per file the base takes in
        let mut homed_ones = Vec::new();
        for (instance, env) in found {
            let InstanceDef::Fn(func) = instance.def else { continue };
            // One the executable has no copy of under its path is left to what calls it.
            let Some(path) = spelling.instance_path(func, instance.args.as_ref()) else {
                continue;
            };
            let Some((_, symbol)) = copies.remove(&path) else { continue };
            homed_ones.push((instance, env, symbol));
        }
        // One reached only through a `dyn`, or a generic caller past the search, is read off
        // rustc's path of it.
        copies.retain(|path, (func, symbol)| {
            if unknown.contains(func) {
                return true;
            }
            let Some(args) = spelling.instance_args(*func, path) else { return true };
            let checked = super::crate_checks_overflow(db, func.krate(db)).unwrap_or(false);
            let instance = Instance { def: InstanceDef::Fn(*func), args: args.store(), checked };
            homed_ones.push((instance, closure_env(db, *func), std::mem::take(symbol)));
            false
        });
        for (instance, env, symbol) in homed_ones {
            let InstanceDef::Fn(func) = instance.def else { continue };
            let mark = closure_mark(db, &instance, &env, self.debug);
            if mark.is_none() {
                unknown.insert(func);
                continue;
            }
            let files = body_files(db, &instance);
            let known = BaseInstance {
                env,
                symbol,
                mark,
                forwarded: false,
                files,
                // alloc: empty, allocates nothing until a parent is noted
                parents: Vec::new(),
                theirs: false,
            };
            self.instances.entry(instance).or_insert(known);
        }
        unknown.extend(copies.into_values().map(|it| it.0));
        for func in generic {
            // alloc: once per generic function of a file the base takes in, kept as its key
            let name = FunctionSignature::of(db, func).name.as_str().to_owned();
            *homed.entry(name).or_insert(true) &= !unknown.contains(&func);
        }
        homed
    }

    /// Takes in the closures rustc compiled of the body of `root`: those it has a symbol of
    /// are instances of the base, which a patch replaces under it.
    fn home_closures(
        &mut self,
        db: &Db,
        // dyn: the build's one resolver, made by blaze-engine or blaze-wasm (which depend on this crate)
        symbols: &dyn SymbolResolver,
        root: FunctionId,
    ) {
        let Some(found) = super::closures_of(db, root) else { return };
        if found.is_empty() {
            return;
        }
        let file = body_file(db, root.into());
        let env = closure_env(db, root);
        // alloc: once per function of a file the base takes in, kept in the base
        let mut unnamed = Vec::new();
        for (expr, closure) in &found {
            let Some((instance, _)) = closure else { continue };
            let mark = closure_mark(db, instance, &env, self.debug);
            match symbols.closure(root, *expr).filter(|_| mark.is_some()) {
                Some(symbol) => {
                    let known = BaseInstance {
                        env,
                        // alloc: the patch's base keeps it
                        symbol: symbol.as_str().to_owned(),
                        mark,
                        forwarded: false,
                        // alloc: the patch's base keeps it
                        files: Some(Box::new([file])),
                        // alloc: empty, allocates nothing until a parent is noted
                        parents: Vec::new(),
                        theirs: false,
                    };
                    self.instances.entry(*instance).or_insert(known);
                }
                None => unnamed.push((*instance, mark)),
            }
        }
        // alloc: once per function of a file the base takes in, kept in the base
        let closures: Vec<_> = found.into_iter().map(|it| it.1).collect();
        let written = match closures.iter().any(Option::is_none) {
            true => super::written_print(db, root),
            false => None,
        };
        self.closures.push(HomedClosures { root, file, closures, unnamed, written });
    }

    /// Whether an edit made what rustc linked something no patch can replace.
    pub(super) fn outdated(&self) -> bool {
        self.outdated
    }

    /// Why what rustc linked can't be patched any longer.
    pub(super) fn why_outdated(&self) -> Option<&'static str> {
        self.outdated_by
    }

    fn outdate_rustc(&mut self, why: &'static str) {
        if self.rustc {
            self.outdated = true;
            self.outdated_by.get_or_insert(why);
        }
    }

    /// Notes that a file changed: one of `crates`, or with `None` one we can't place.
    pub(super) fn touch(&mut self, db: &Db, crates: Option<&[Crate]>) {
        // rustc's copies of what sees the file are of before, and nothing here says which.
        self.outdate_rustc("an edit that is more than the bodies of functions");
        let Some(crates) = crates else {
            self.touched.any = true;
            return;
        };
        for &krate in crates {
            if !self.touched.crates.contains(&krate) {
                self.touched.crates.extend(krate.transitive_rev_deps(db));
            }
        }
    }

    /// Notes that functions were added to a file of `crates`, or taken away, and nothing else of
    /// it changed but bodies that nothing else sees into, where no other crate depends on
    /// `crates`, no file of them named the new functions before (or names those taken away
    /// after), and `files` are their files. What rustc compiled of them is then known when
    /// [`Self::home`] took in every one of those files.
    pub(super) fn touch_added(&mut self, db: &Db, crates: &[Crate], files: &[FileId]) {
        if !files.iter().all(|it| self.homed.contains(it)) {
            return self.touch(db, Some(crates));
        }
        self.touched.crates.extend(crates.iter().copied());
    }

    /// Notes that a file changed in the bodies of functions alone, none of which the rest of
    /// the program sees into.
    pub(super) fn touch_bodies(&mut self, file: FileId) {
        self.touched.bodies.insert(file);
        if !self.homed.contains(&file) {
            self.outdate_rustc("a file whose functions were not looked at before it changed");
        }
    }

    /// Takes what every part of [`Base::stale`] found as where the next edit starts from.
    pub(super) fn settle(&mut self, db: &Db, found: &mut [Stale]) {
        self.their_statics(db, found);
        self.touched.settle(found);
    }

    /// Declines when the functions the base left to rustc reach other statics than they did.
    /// Walking to them is slow, and nothing changes where the walk goes but the code of what
    /// it went through.
    fn their_statics(&mut self, db: &Db, found: &mut [Stale]) {
        let check_all = std::env::var_os("BLAZE_CHECK_ALL").is_some();
        let changed = found.iter().any(|it| it.their_walk_changed);
        let Some(first) = found.first_mut() else { return };
        if !changed && !check_all {
            return;
        }
        let start = stdx::time::Instant::now();
        let now = probe(|| TheirStatics::walk(db, self.from_rustc.iter().copied()));
        match now {
            Some((now, walked)) if now == self.their_statics => {
                if changed {
                    self.their_walk =
                        TheirWalk::new(db, &now, walked, &mut self.instances, &mut self.statics);
                }
            }
            _ if !changed => eprintln!("check-all: MISSED statics of the functions left to rustc"),
            _ => first.declined = Some("the statics of a function left to rustc changed"),
        }
        first.steps.left += start.elapsed();
    }

    /// The roots an edit can have changed, or all of them, one part in `parts`.
    fn roots_to_ask(
        &self,
        all: bool,
        part: usize,
        parts: usize,
    ) -> impl Iterator<Item = usize> + '_ {
        let touched = &self.touched;
        if all || touched.any {
            return Either::Left((part..self.compiled.len()).step_by(parts));
        }
        let homes = &self.homes;
        let of_crates = touched.crates.iter().filter_map(|it| homes.roots_of_crate.get(it));
        let of_files = touched.bodies.iter().filter_map(|it| homes.roots_of_file.get(it));
        // alloc: once per part of a look, the work list `part_of` sorts
        let mut asked: Vec<u32> = of_crates.chain(of_files).flatten().copied().collect();
        asked.extend(touched.roots.iter().filter_map(|it| homes.root.get(it)));
        if touched.edited() {
            asked.extend(&self.cached_roots);
        }
        Either::Right(part_of(asked, part, parts))
    }

    /// The instances an edit can have changed, or all of them, one part in `parts`.
    fn instances_to_ask(
        &self,
        all: bool,
        part: usize,
        parts: usize,
    ) -> impl Iterator<Item = usize> + '_ {
        let touched = &self.touched;
        if all || touched.any {
            return Either::Left((part..self.instances.len()).step_by(parts));
        }
        let homes = &self.homes;
        let of_crates = touched.crates.iter().filter_map(|it| homes.instances_of_crate.get(it));
        let of_files = touched.bodies.iter().filter_map(|it| homes.instances_of_file.get(it));
        // alloc: once per part of a look, the work list `part_of` sorts
        let mut asked: Vec<u32> = of_crates.chain(of_files).flatten().copied().collect();
        if !touched.bodies.is_empty() {
            asked.extend(&homes.instances_of_any_file);
        }
        if touched.edited() {
            asked.extend(&self.cached_instances);
        }
        asked.extend(
            touched
                .instances
                .iter()
                .filter_map(|it| self.instances.get_index_of(it))
                .map(|it| it as u32),
        );
        Either::Right(part_of(asked, part, parts))
    }

    /// What one part in `parts` of the base no longer is what it was. Looking is most of the
    /// work of a patch, and the parts can be looked at on a thread each.
    pub(super) fn stale(&self, db: &Db, part: usize, parts: usize) -> Stale {
        let start = stdx::time::Instant::now();
        let mut stale = Stale::default();
        let every = |len: usize| (part..len).step_by(parts);
        // A check of the skipping below: looks at what no edit can have changed too, and says
        // so if one did.
        let check_all = std::env::var_os("BLAZE_CHECK_ALL").is_some();
        for i in every(self.rejected.len()) {
            let (instance, env) = &self.rejected[i];
            let compiles = probe(|| compiled_instance(db, *instance, *env).is_ok());
            if compiles != Some(false) {
                return stale.declined("an instance that did not compile is another now");
            }
        }
        for i in self.roots_to_ask(check_all, part, parts) {
            let root = self.compiled[i].0;
            let touched = self.touched.reaches(self.crates[i])
                || self.touched.reaches_bodies(Some(&[self.files[i]]))
                || self.touched.roots.contains(&root)
                || self.touched.edited() && self.cached_roots.binary_search(&(i as u32)).is_ok();
            if !touched && !check_all {
                continue;
            }
            let asked = stdx::time::Instant::now();
            let same = probe(|| Some(Mark::of_root(db, root, self.debug)? == self.marks[i]));
            stale.steps.roots_asked += 1;
            stale.steps.slowest = stale.steps.slowest.max(asked.elapsed());
            match same {
                Some(Some(true)) => {}
                _ if !touched => {
                    eprintln!("check-all: MISSED root {root:?} of {:?}", self.crates[i]);
                    continue;
                }
                Some(Some(false)) => stale.roots.push(root),
                Some(None) => return stale.declined("a root of the base no longer compiles"),
                None => stale.gone.push(root),
            }
        }
        for (root, was, file) in self.unnamed.iter().skip(part).step_by(parts) {
            let touched =
                self.touched.reaches(root.krate(db)) || self.touched.reaches_bodies(Some(&[*file]));
            if !touched {
                continue;
            }
            let mark = probe(|| Mark::of_root(db, *root, self.debug));
            let Some(mark) = mark else {
                stale.gone.push(*root);
                continue;
            };
            // One that did not compile then can't be told to be the same.
            if !was.zip(mark).is_some_and(|(was, now)| was.same_code(now)) {
                return stale.declined("a function that rustc's symbols name no copy of changed");
            }
        }
        for homed in self.closures.iter().skip(part).step_by(parts) {
            let touched = self.touched.reaches(homed.root.krate(db))
                || self.touched.reaches_bodies(Some(&[homed.file]));
            if !touched {
                continue;
            }
            let now = probe(|| {
                let now = super::closures_of(db, homed.root)?;
                Some(now.iter().map(|it| &it.1).eq(&homed.closures))
            });
            let Some(now) = now else {
                stale.gone.push(homed.root);
                continue;
            };
            // An `async` block has no mark here to tell a change by, but where and how the
            // root is written.
            let asynchronous = homed.closures.iter().any(Option::is_none);
            let rewritten = || {
                homed.written.is_none()
                    || probe(|| super::written_print(db, homed.root)).flatten() != homed.written
            };
            if now != Some(true) || (asynchronous && rewritten()) {
                return stale.declined("a closure in what rustc linked is another now");
            }
            let env = closure_env(db, homed.root);
            for (instance, was) in &homed.unnamed {
                let mark = probe(|| closure_mark(db, instance, &env, self.debug));
                if !was.zip(mark.flatten()).is_some_and(|(was, now)| was.same_code(now)) {
                    return stale
                        .declined("a closure that rustc's symbols name no copy of changed");
                }
            }
        }
        stale.steps.roots = start.elapsed();
        for i in self.instances_to_ask(check_all, part, parts) {
            let Some((instance, was)) = self.instances.get_index(i) else { continue };
            let touched = self.touched.reaches(was.env.krate)
                || self.touched.reaches_bodies(was.files.as_deref())
                || self.touched.instances.contains(instance)
                || self.touched.edited()
                    && self.cached_instances.binary_search(&(i as u32)).is_ok();
            if !touched && !check_all {
                continue;
            }
            let asked = stdx::time::Instant::now();
            let mark = probe(|| Mark::of_instance(db, instance, &was.env, self.debug));
            stale.steps.instances_asked += 1;
            stale.steps.slowest = stale.steps.slowest.max(asked.elapsed());
            stale.their_walk_changed |= touched
                && was.theirs
                && mark.flatten().map(|it| it.serial) != was.mark.map(|it| it.serial);
            match mark {
                Some(mark) if mark == was.mark => {}
                _ if !touched => {
                    eprintln!("check-all: MISSED instance {instance:?} of {:?}", was.env.krate);
                    continue;
                }
                Some(Some(_)) if !was.forwarded => stale.instances.push(*instance),
                // One that had no code of its own might have now.
                _ => stale.broken.push((
                    Node::Instance(*instance),
                    "an instance of the base gained or lost code of its own",
                )),
            }
        }
        stale.steps.instances = start.elapsed() - stale.steps.roots;
        for (&id, was) in self.statics.iter().skip(part).step_by(parts) {
            let touched = self.touched.reaches(was.krate)
                || self.touched.reaches_bodies(Some(&[was.file]))
                || self.touched.statics.contains(&id);
            if !touched && !check_all {
                continue;
            }
            let mark = probe(|| {
                let compiled = compiled_static(db, id).as_ref().ok()?;
                Some(Mark::new(db, static_serial(db, id), &compiled.requests))
            });
            if mark == Some(Some(was.mark)) {
                continue;
            }
            if !touched {
                eprintln!("check-all: MISSED static {id:?} of {:?}", was.krate);
                continue;
            }
            // rustc's code reads rustc's copy, which a patch has no copy of its own beside.
            if self.rustc {
                return stale.declined("a static changed, and rustc's copy is the program's");
            }
            let writable = probe(|| compiled_static(db, id).as_ref().ok().map(|it| it.writable));
            // rustc's copy holds what the program wrote, from the first value it had: one of
            // ours beside it would split the state, and it can't start again from another. A
            // static the edit took away (or left with no value) has no copy of ours to split
            // it with: nothing compiled now names it.
            if was.shared && matches!(writable, Some(Some(_))) {
                return stale.declined(
                    "the first value changed of a static with state that rustc's code shares",
                );
            }
            stale.their_walk_changed |= was.theirs;
            // rustc's code has rustc's copy, and a thread's copy is laid out by the linker.
            if writable == Some(Some(was.writable)) && !was.theirs && !is_thread_local(db, id) {
                stale.statics.push(id);
            } else {
                stale.broken.push((Node::Static(id), "a static of the base changed"));
            }
        }
        stale.steps.statics = start.elapsed() - stale.steps.roots - stale.steps.instances;
        for i in every(self.skipped.len()) {
            let Some(was) = &self.left[i] else { continue };
            let root = self.skipped[i].0;
            let touched = self.touched.reaches(root.krate(db))
                || self.touched.reaches_bodies(Some(&[body_file(db, root.into())]));
            if !touched && !check_all {
                continue;
            }
            let Some(still) = probe(|| was.still(db, root)) else {
                stale.gone.push(root);
                continue;
            };
            // Without code to tell a change by, any edit that reaches it may be one.
            if was.code.is_none() && touched || !still {
                if !touched {
                    eprintln!("check-all: MISSED function left to rustc {root:?}");
                    continue;
                }
                return stale.declined("a function left to rustc changed");
            }
        }
        for i in every(self.their_walk.instances.len()) {
            let was = &self.their_walk.instances[i];
            if !self.touched.reaches(was.env.krate)
                && !self.touched.reaches_bodies(was.files.as_deref())
            {
                continue;
            }
            let serial = probe(|| {
                let compiled = compiled_instance(db, was.instance, was.env);
                compiled.as_ref().ok().map(|_| instance_serial(db, was.instance, was.env))
            });
            stale.their_walk_changed |= serial != Some(was.serial);
        }
        for i in every(self.their_walk.statics.len()) {
            let was = &self.their_walk.statics[i];
            if !self.touched.reaches(was.krate) && !self.touched.reaches_bodies(Some(&[was.file])) {
                continue;
            }
            let serial = probe(|| {
                compiled_static(db, was.id).as_ref().ok().map(|_| static_serial(db, was.id))
            });
            stale.their_walk_changed |= serial != Some(was.serial);
        }
        stale.steps.left =
            start.elapsed() - stale.steps.roots - stale.steps.instances - stale.steps.statics;
        stale
    }

    /// Whether a root still refers to the node, directly or through others.
    fn reaches(&self, db: &Db, node: &Node, unchanged: &Unchanged) -> bool {
        let mut seen = FxHashSet::default();
        let mut stack: SmallVec<[&Node; 16]> = SmallVec::new();
        stack.push(node);
        while let Some(node) = stack.pop() {
            let parents = match node {
                Node::Root(_) => return true,
                Node::Instance(instance) => self.instances.get(instance).map(|it| &it.parents),
                Node::Static(id) => self.statics.get(id).map(|it| &it.parents),
            };
            for parent in parents.into_iter().flatten() {
                if !seen.contains(parent) && self.still_asks_for(db, parent, node, unchanged) {
                    seen.insert(parent);
                    stack.push(parent);
                }
            }
        }
        false
    }

    /// Those of `changed` whose value no longer fits where the base has it, with what of the
    /// base refers to each. The patch has a copy of its own of such a static, so what refers
    /// to the base's goes in the patch to refer to that; why not, when something no patch
    /// compiles again refers to one.
    fn moved(
        &self,
        db: &Db,
        changed: &[StaticId],
    ) -> std::result::Result<(FxHashSet<StaticId>, Vec<Node>), &'static str> {
        // alloc: the result, empty until a static outgrew its copy
        let (mut moved, mut referrers) = (FxHashSet::default(), Vec::new());
        for &id in changed {
            let Some(was) = self.statics.get(&id) else { continue };
            let now = probe(|| compiled_static(db, id).as_ref().ok().map(CompiledStatic::layout));
            // One with no value now is refused as it is compiled.
            let fits = match (was.layout, now.flatten()) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some((size, align)), Some((now, aligned))) => now <= size && aligned <= align,
            };
            if fits {
                continue;
            }
            for parent in &was.parents {
                match parent {
                    Node::Root(_) => {}
                    Node::Instance(instance) => {
                        let known = self.instances.get(instance);
                        if known.is_none_or(|it| it.mark.is_none() || it.forwarded) {
                            return Err("a static that outgrew the base's copy is referred to by \
                                        an instance the patch can't compile again");
                        }
                    }
                    Node::Static(it) => {
                        let known = self.statics.get(it);
                        if known.is_none_or(|it| it.theirs || it.shared) || is_thread_local(db, *it)
                        {
                            return Err("a static that outgrew the base's copy is stored in one \
                                        the patch can't write");
                        }
                    }
                }
                referrers.push(*parent);
            }
            moved.insert(id);
        }
        Ok((moved, referrers))
    }

    /// Each of `dropped` by the instances and statics of the base above it that the edit left
    /// as they were, up to what it changed: what refers to one of those reaches the base's
    /// copy of the dropped one through it.
    fn behind(&self, dropped: &[Node], unchanged: &Unchanged) -> FxHashMap<Node, Vec<Node>> {
        let mut behind: FxHashMap<Node, Vec<Node>> = FxHashMap::default();
        for node in dropped {
            let mut seen = FxHashSet::default();
            let mut stack: SmallVec<[&Node; 16]> = SmallVec::new();
            stack.push(node);
            while let Some(below) = stack.pop() {
                let parents = match below {
                    Node::Root(_) => None,
                    Node::Instance(instance) => self.instances.get(instance).map(|it| &it.parents),
                    Node::Static(id) => self.statics.get(id).map(|it| &it.parents),
                };
                for parent in parents.into_iter().flatten() {
                    if matches!(parent, Node::Root(_))
                        || unchanged.changed.contains(parent)
                        || !seen.insert(parent)
                    {
                        continue;
                    }
                    behind.entry(*parent).or_default().push(*node);
                    stack.push(parent);
                }
            }
        }
        behind
    }

    /// Whether what referred to the node in the base does so after the edit.
    fn still_asks_for(
        &self,
        db: &Db,
        parent: &Node,
        node: &Node,
        unchanged: &Unchanged,
    ) -> bool {
        // A broken parent that is reached stops the patch by itself.
        if unchanged.broken.contains(parent) {
            return false;
        }
        if matches!(parent, Node::Root(root) if unchanged.gone.contains(root)) {
            return false;
        }
        if !unchanged.changed.contains(parent) {
            return true;
        }
        let requests = match parent {
            Node::Root(root) => compiled_root(db, *root).as_ref().ok().map(|it| &it.requests),
            Node::Instance(instance) => self.instances.get(instance).and_then(|known| {
                let compiled = compiled_instance(db, *instance, known.env);
                compiled.as_ref().ok().map(|it| &it.requests)
            }),
            Node::Static(id) => compiled_static(db, *id).as_ref().ok().map(|it| &it.requests),
        };
        requests.is_some_and(|requests| node.is_asked_for_by(requests))
    }

    /// The futures that functions left to rustc handle, or `None` when that can be any.
    fn their_async_blocks(&self) -> Option<Option<FxHashSet<salsa::Id>>> {
        let mut theirs = Some(FxHashSet::default());
        let mut any = false;
        for left in self.left.iter().flatten() {
            any = true;
            theirs = theirs.zip(left.async_blocks.as_ref()).map(|(mut all, blocks)| {
                all.extend(blocks);
                all
            });
        }
        any.then_some(theirs)
    }

    /// An object with what changed since the base was built, or why the change is more than
    /// a patch can hold.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn patch(
        &self,
        db: &Db,
        // dyn: the build's one resolver, made by blaze-engine or blaze-wasm (which depend on this crate)
        symbols: &dyn SymbolResolver,
        roots: Roots<'_>,
        on_demand: &FxHashSet<FunctionId>,
        denied: &FxHashSet<FunctionId>,
        known: &Rejections,
        own_statics: OwnStatics,
        stale: Vec<Stale>,
    ) -> Result<std::result::Result<Patch, &'static str>> {
        let start = stdx::time::Instant::now();
        // A function added where every function is compiled whether called or not, as all of
        // a program's are when its `main` is C's, is compiled into the patch with the rest.
        let added: FxHashSet<FunctionId> = match self.rustc {
            true => FxHashSet::default(),
            false => {
                // alloc: once per patch, the entries of the base
                let base: FxHashSet<FunctionId> = self.entries.iter().copied().collect();
                // alloc: once per patch, the entries now
                let now: FxHashSet<FunctionId> = roots.entries.iter().copied().collect();
                if !base.is_subset(&now) {
                    return Ok(Err("a root compiled whether called or not is gone"));
                }
                // alloc: once per patch, the entries added
                now.difference(&base).copied().collect()
            }
        };
        // Every function is in rustc's executable: one a patch calls is called there, and only
        // what changed is compiled.
        let none = FxHashSet::default();
        let on_demand = if self.rustc { &none } else { on_demand };
        let all: FxHashSet<FunctionId> =
            // alloc: once per patch, the roots now
            roots.entries.iter().chain(roots.on_demand).copied().collect();
        let known_roots =
            || self.compiled.iter().map(|it| it.0).chain(self.skipped.iter().map(|it| it.0));
        // One the look found gone is one the program no longer lists, and the other way round.
        let gone: FxHashSet<FunctionId> =
            // alloc: once per patch, what the look found gone
            stale.iter().flat_map(|it| it.gone.iter().copied()).collect();
        if gone.iter().any(|root| all.contains(root)) {
            return Ok(Err("a root of the base no longer compiles"));
        }
        if !known_roots().all(|root| all.contains(&root) || gone.contains(&root)) {
            return Ok(Err("a root of the base is gone"));
        }

        let sum = |len: fn(&Stale) -> usize| stale.iter().map(len).sum::<usize>();
        let mut changed_roots = Vec::with_capacity(sum(|it| it.roots.len()));
        let mut changed_instances = Vec::with_capacity(sum(|it| it.instances.len()));
        let mut changed_statics = Vec::with_capacity(sum(|it| it.statics.len()));
        let mut broken = Vec::with_capacity(sum(|it| it.broken.len()));
        for part in stale {
            if let Some(why) = part.declined {
                return Ok(Err(why));
            }
            changed_roots.extend(part.roots);
            changed_instances.extend(part.instances);
            changed_statics.extend(part.statics);
            broken.extend(part.broken);
        }
        // A function at a level above 1 differs from the base, whose code is Cranelift's.
        for node in known.tiers.nodes() {
            match &node.0 {
                super::tier::Kind::Root(root) => {
                    if self.homes.root.contains_key(root)
                        && !changed_roots.contains(root)
                        && !gone.contains(root)
                    {
                        changed_roots.push(*root);
                    }
                }
                super::tier::Kind::Instance(instance, env) => {
                    let known = self.instances.get(instance);
                    if known.is_some_and(|it| it.env == *env && it.mark.is_some() && !it.forwarded)
                        && !changed_instances.contains(instance)
                    {
                        changed_instances.push(*instance);
                    }
                }
            }
        }
        let (moved, referrers) = match self.moved(db, &changed_statics) {
            Ok(it) => it,
            Err(why) => return Ok(Err(why)),
        };
        for node in referrers {
            match node {
                Node::Root(root) if !gone.contains(&root) && !changed_roots.contains(&root) => {
                    changed_roots.push(root)
                }
                Node::Instance(instance) if !changed_instances.contains(&instance) => {
                    changed_instances.push(instance)
                }
                Node::Static(id) if !changed_statics.contains(&id) => changed_statics.push(id),
                _ => {}
            }
        }
        // What the edit took away is still in the base, where nothing refers to it any more.
        let unchanged = Unchanged {
            changed: changed_roots
                .iter()
                .map(|&root| Node::Root(root))
                .chain(changed_instances.iter().copied().map(Node::Instance))
                .chain(changed_statics.iter().map(|&id| Node::Static(id)))
                // alloc: once per patch, what it changed
                .collect(),
            // alloc: once per patch, what it can't replace
            broken: broken.iter().map(|it| it.0).collect(),
            gone,
            behind: FxHashMap::default(),
            moved,
        };
        // Nothing tells what in rustc's code refers to what.
        if let Some((_, why)) =
            broken.iter().find(|it| self.rustc || self.reaches(db, &it.0, &unchanged))
        {
            return Ok(Err(why));
        }
        // alloc: once per patch, what it leaves uncalled
        let mut dropped: Vec<Node> = broken.iter().map(|it| it.0).collect();
        // rustc's code may call any of them still.
        if !self.rustc {
            changed_instances.retain(|it| {
                let node = Node::Instance(*it);
                let reached = self.reaches(db, &node, &unchanged);
                if !reached {
                    dropped.push(node);
                }
                reached
            });
            changed_statics.retain(|&it| {
                let reached = self.reaches(db, &Node::Static(it), &unchanged);
                if !reached {
                    dropped.push(Node::Static(it));
                }
                reached
            });
        }
        let behind = self.behind(&dropped, &unchanged);
        let verify = start.elapsed();

        let mut cg = ObjectCodegen::new(
            db,
            symbols,
            &known.rejected,
            own_statics,
            on_demand,
            Some(self),
            false,
        )?;
        cg.phases.compile = verify;
        cg.phases.setup = start.elapsed() - verify;
        // clone: once per patch, the object's codegen owns its set
        cg.rustc_futures = known.futures.clone();
        cg.rustc_values = known.frozen.rustc_values();
        cg.tiers = Some(&known.tiers);
        // What nothing reached any more can be reached again by the code compiled now.
        cg.replaced = Unchanged { behind, ..unchanged };
        // rustc's copies are the ones the base was built against: what they call is in it.
        for instance in &changed_instances {
            let was = &self.instances[instance];
            let Ok(compiled) = compiled_instance(db, *instance, was.env).as_ref() else {
                return Ok(Err("an instance of the base no longer compiles"));
            };
            let id = cg.module.declare_function(
                &was.symbol,
                Linkage::Hidden,
                &compiled.code.signature,
            )?;
            cg.instances.insert(*instance, id);
            cg.pending.push((*instance, was.env, id));
        }
        for &id in &changed_statics {
            match cg.static_data(id) {
                Ok(_) => {}
                Err(e @ CodegenError::Refused(_)) => return Err(e),
                Err(_) => return Ok(Err("a static of the base no longer has a value")),
            }
        }
        // rustc's callers of one that hands out a value of a type only its body names go on
        // calling or polling rustc's code for that type, which is of the source before.
        if self.rustc && changed_roots.iter().any(|&root| super::hands_out_opaque(db, root)) {
            return Ok(Err("a function that hands rustc's code an `impl Trait` changed or moved"));
        }
        if self.rustc {
            // alloc: once per patch of what rustc linked
            let crates = changed_roots.iter().map(|it| it.krate(db)).collect();
            cg.define_rustc_allocator_entries(&crates)?;
        }
        // alloc: once per patch, what is queued already
        let mut queued: FxHashSet<FunctionId> = known_roots().collect();
        let mut queue = changed_roots;
        queue.extend(added.iter().copied().filter(|&func| queued.insert(func)));
        queue.extend(cg.wanted.drain(..).filter(|&func| queued.insert(func)));
        let mut next = 0;
        loop {
            while let Some(&root) = queue.get(next) {
                next += 1;
                if denied.contains(&root) || known.left.contains(&root) {
                    return Ok(Err("a root can't be compiled"));
                }
                match cg.define_function(root) {
                    Ok(_) => {}
                    // As in a full build, a function added to a crate whose symbols are
                    // rustc's has none to be put in the object under: what calls it has a
                    // copy of its own.
                    Err(e) if e.no_symbol() && added.contains(&root) => {}
                    Err(e @ CodegenError::Refused(_)) => return Err(e),
                    Err(_) => return Ok(Err("a root can't be compiled")),
                }
                queue.extend(cg.wanted.drain(..).filter(|&func| queued.insert(func)));
            }
            let failed = cg.define_instances();
            if let Some(refused) = cg.refused.take() {
                return Err(refused);
            }
            if !failed.is_empty() {
                return Ok(Err("an instance can't be compiled"));
            }
            queue.extend(cg.wanted.drain(..).filter(|&func| queued.insert(func)));
            if next == queue.len() {
                break;
            }
        }
        // A future is laid out by one compiler: a function that now handles one of rustc's
        // has to be left to rustc as well.
        if let Some(theirs) = self.their_async_blocks() {
            let shares = |root| match (async_blocks(db, root), &theirs) {
                (Some(ours), Some(theirs)) => !ours.is_disjoint(theirs),
                (Some(ours), None) => !ours.is_empty(),
                (None, _) => true,
            };
            if queue.iter().copied().any(shares) {
                return Ok(Err("a function now shares an `async` block with one left to rustc"));
            }
        }
        if std::env::var_os("BLAZE_STATS").is_some() {
            let decls = cg.module.declarations();
            let anew = (cg.instances.values())
                .filter(|&&id| {
                    decls.get_function_decl(id).linkage_name(id).starts_with("blaze.patch::")
                })
                .count();
            eprintln!("patch: {anew} instances the base has no code for");
        }
        let (bodies, mut phases) = (cg.bodies, cg.phases);
        let start = stdx::time::Instant::now();
        let (object, _) = cg.finish()?;
        phases.object = start.elapsed();
        Ok(Ok(Patch { object, bodies, phases }))
    }
}

#[cfg(test)]
mod tests {
    use super::probe;

    #[test]
    fn a_program_started_again_keeps_what_rustcs_copies_are_of() {
        let file = base_db::FileId::from_raw(7);
        let mut known = crate::Rejections { patching: true, full: true, ..Default::default() };
        known.frozen.edit_bodies(file);
        known.start_again();
        assert!(known.patching && !known.full);
        assert!(known.frozen.edited_bodies.contains(&file));
    }

    #[test]
    fn a_probe_that_is_interrupted_goes_on_unwinding_and_one_that_panics_finds_nothing() {
        let gone: Option<()> = probe(|| panic!("the base no longer has it"));
        assert_eq!(gone, None);
        assert_eq!(probe(|| 7), Some(7));
        let waited: Option<()> =
            probe(|| std::panic::resume_unwind(Box::new(salsa::Cancelled::PropagatedPanic)));
        assert_eq!(waited, None);
        let interrupted = std::panic::catch_unwind(|| {
            probe(|| std::panic::resume_unwind(Box::new(salsa::Cancelled::PendingWrite)))
        });
        let stopped: Box<dyn std::any::Any + Send> = interrupted.unwrap_err();
        assert!(stopped.is::<salsa::Cancelled>());
    }
}
