//! This crate defines the basic database trait for interacting with source code using [`salsa`].
//!
//! The concrete implementation DB is defined by ide.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![forbid(unsafe_code)]

#[cfg(feature = "in-rust-tree")]
extern crate rustc_driver as _;

pub use salsa;
use span::TextSize;

mod change;
mod content;
mod editioned_file_id;
mod files;
mod input;
mod table;
pub mod target;
#[cfg(test)]
mod tests;
mod view;

use std::{
    cell::RefCell,
    panic,
    str::FromStr as _,
    sync::{Once, atomic::AtomicUsize},
};

pub use crate::{
    change::FileChange,
    content::{CFG_OPTIONS_DOMAIN, InputId, cfg_options_content},
    editioned_file_id::EditionedFileId,
    files::Files,
    input::{
        BuiltCrateData, BuiltDependency, Crate, CrateBuilder, CrateBuilderId, CrateDataBuilder,
        CrateDisplayName, CrateGraphBuilder, CrateName, CrateOrigin, CrateType, CrateTypes,
        CratesIdMap, CratesMap,
        DependencyBuilder, Env, ExtraCrateData, LangCrateOrigin, ProcMacroLoadingError,
        ProcMacroPaths, ReleaseChannel, SourceRoot, SourceRootId, TargetKind, UniqueCrateData,
    },
    view::{ViewChange, ViewError, ViewFile, ViewRoot},
};
pub use blaze_ids::ContentId;
use blaze_ids::Hasher;
use rustc_hash::FxHashSet;
use salsa::{Durability, Setter};
pub use semver::{BuildMetadata, Prerelease, Version, VersionReq};
use triomphe::Arc;
pub use vfs::{AbsPathBuf, AnchoredPath, AnchoredPathBuf, FileId, VfsPath, file_set::FileSet};

pub type FxIndexSet<T> = indexmap::IndexSet<T, rustc_hash::FxBuildHasher>;
pub type FxIndexMap<K, V> =
    indexmap::IndexMap<K, V, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;

#[macro_export]
macro_rules! impl_intern_key {
    ($id:ident, $loc:ident) => {
        #[salsa::interned(unsafe(no_lifetime), revisions = usize::MAX)]
        #[derive(PartialOrd, Ord)]
        pub struct $id {
            #[returns(ref)]
            pub loc: $loc,
        }

        // If we derive this salsa prints the values recursively, and this causes us to blow.
        impl ::std::fmt::Debug for $id {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.debug_tuple(stringify!($id))
                    .field(&format_args!("{:04x}", self.0.index()))
                    .finish()
            }
        }
    };
}

pub const DEFAULT_FILE_TEXT_LRU_CAP: u16 = 16;
pub const DEFAULT_PARSE_LRU_CAP: u16 = 128;
pub const DEFAULT_BORROWCK_LRU_CAP: u16 = 2024;

/// The set of roots for crates.io libraries.
/// Files in libraries are assumed to never change.
#[salsa::input(singleton, debug)]
pub struct LibraryRoots {
    #[returns(ref)]
    pub roots: FxHashSet<SourceRootId>,
}

/// The set of "local" (that is, from the current workspace) roots.
/// Files in local roots are assumed to change frequently.
#[salsa::input(singleton, debug)]
pub struct LocalRoots {
    #[returns(ref)]
    pub roots: FxHashSet<SourceRootId>,
}

/// The variables of the process a build is for, which `env!` and `option_env!` read where
/// the crate's own have nothing: those of whoever asked for the build.
/// Its content ID is [`Env::content`] of its variables.
#[salsa::input(singleton, debug)]
pub struct ProcessEnv {
    #[returns(ref)]
    #[get(vars_field)]
    #[set(set_vars_field)]
    vars: Env,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl ProcessEnv {
    pub fn vars<'db, Db: ?Sized + salsa::Database>(self, db: &'db Db) -> &'db Env {
        self.vars_field(db)
    }

    pub fn content<Db: ?Sized + salsa::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

/// The variable `key` of the process the build is for.
pub fn process_env_var(db: &dyn salsa::Database, key: &str) -> Option<String> {
    ProcessEnv::try_get(db)?.vars(db).get(key)
}

/// Has builds from here on be for a process with the variables `vars`.
pub fn set_process_env(db: &mut dyn salsa::Database, vars: Env) {
    let content = InputId(vars.content());
    match ProcessEnv::try_get(&*db) {
        Some(env) => {
            env.set_vars_field(db).to(vars);
            if env.content_field(&*db) != content {
                env.set_content_field(db).to(content);
            }
        }
        None => {
            ProcessEnv::new(&*db, vars, content);
        }
    }
}

/// A file's text. Set only through [`Files`] (and the [`SourceDatabase`] methods that call it),
/// which keeps [`FileText::content`] the content ID of the text and its appended module.
#[salsa::input(debug)]
pub struct FileText {
    #[returns(ref)]
    #[get(text_field)]
    #[set(set_text_field)]
    text: std::sync::Arc<str>,
    pub file_id: vfs::FileId,
    /// The module a crate root's text ends with that the compiler wrote, not the file on disk:
    /// blaze's test harness or list of proc macros. rustc injects its own after it applies
    /// the crate's `#![cfg]`, so a root that `#![cfg]` takes away still has it.
    #[default]
    #[returns(copy)]
    #[get(appended_root_module_field)]
    #[set(set_appended_root_module_field)]
    appended_root_module: Option<&'static str>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl FileText {
    /// A file text's content ID is of the blaze-vfs ID of its text's bytes
    /// (`ContentId::of(blaze_vfs::FILE_DOMAIN, ..)`), then of its appended root module (0, or 1
    /// and the module), which changes what the crate root is. Building on the vfs ID means a
    /// view's file, whose vfs ID is given and checked, is hashed once. The file's ID and path
    /// are not in it.
    pub const DOMAIN: &'static str = "blaze v2 file text 2";
    /// The content ID a file of a source root has when it has no text (bytes that are not
    /// UTF-8, or none loaded): what the front end sees of it is that it has none.
    pub const NO_TEXT_DOMAIN: &'static str = "blaze v2 file without text 1";

    pub fn content_of(text: &str, appended_root_module: Option<&str>) -> ContentId {
        FileText::content_of_file(
            ContentId::of(blaze_vfs::FILE_DOMAIN, text.as_bytes()),
            appended_root_module,
        )
    }

    /// [`FileText::content_of`] from the blaze-vfs ID of the text's bytes, without reading them.
    pub fn content_of_file(file: ContentId, appended_root_module: Option<&str>) -> ContentId {
        let mut hasher = Hasher::new(FileText::DOMAIN);
        hasher.id(&file);
        content::opt(&mut hasher, appended_root_module);
        hasher.finish()
    }

    /// What [`FileText::content_of_file`] hashes under [`FileText::DOMAIN`], appended to `out`:
    /// for hashing many in one batch.
    pub(crate) fn content_bytes(file: &ContentId, appended_root_module: Option<&str>, out: &mut Vec<u8>) {
        out.extend_from_slice(&file.0);
        content::opt_bytes(out, appended_root_module);
    }

    pub fn no_text() -> ContentId {
        files::no_text()
    }

    /// The text, which the file's parse tree shares ([`EditionedFileId::parse`]).
    pub fn text<'db, Db: ?Sized + salsa::Database>(
        self,
        db: &'db Db,
    ) -> &'db std::sync::Arc<str> {
        self.text_field(db)
    }

    /// See the field: the module the compiler appends to a crate root.
    pub fn appended_root_module<Db: ?Sized + salsa::Database>(
        self,
        db: &Db,
    ) -> Option<&'static str> {
        self.appended_root_module_field(db)
    }

    pub fn content<Db: ?Sized + salsa::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

/// The contents of a file named by `include_bytes!` or `include_str!`, `None` if it can't be read.
#[salsa::input(debug)]
pub struct IncludedFile {
    #[returns(ref)]
    #[get(bytes_field)]
    #[set(set_bytes_field)]
    bytes: Option<std::sync::Arc<[u8]>>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl IncludedFile {
    /// Of its bytes (1 and the bytes), or that it could not be read (0).
    pub const DOMAIN: &'static str = "blaze v2 included file 1";

    pub fn content_of(bytes: Option<&[u8]>) -> ContentId {
        let mut hasher = Hasher::new(IncludedFile::DOMAIN);
        match bytes {
            None => hasher.update(&[0]),
            Some(bytes) => hasher.update(&[1]).part(bytes),
        };
        hasher.finish()
    }

    pub fn bytes<'db, Db: ?Sized + salsa::Database>(
        self,
        db: &'db Db,
    ) -> &'db Option<std::sync::Arc<[u8]>> {
        self.bytes_field(db)
    }

    pub fn content<Db: ?Sized + salsa::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

#[salsa::input(debug)]
pub struct FileSourceRootInput {
    #[returns(copy)]
    pub source_root_id: SourceRootId,
}

/// A source root. Its content ID is of its files' paths and content IDs and `is_library`
/// ([`SourceRoot::DOMAIN`]), kept by [`Files`] as either changes.
#[salsa::input(debug)]
pub struct SourceRootInput {
    #[returns(ref)]
    #[get(source_root_field)]
    #[set(set_source_root_field)]
    source_root: Arc<SourceRoot>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl SourceRootInput {
    pub fn source_root<Db: ?Sized + salsa::Database>(self, db: &Db) -> Arc<SourceRoot> {
        self.source_root_field(db).clone()
    }

    /// [`SourceRootInput::source_root`] without a clone of the `Arc` (a write to the count
    /// every reader of the root shares).
    pub fn source_root_ref<'db, Db: ?Sized + salsa::Database>(self, db: &'db Db) -> &'db SourceRoot {
        self.source_root_field(db)
    }

    pub fn content<Db: ?Sized + salsa::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

#[salsa::db]
pub trait SourceDatabase: salsa::Database + std::fmt::Debug {
    /// Text of the file.
    fn file_text(&self, file_id: vfs::FileId) -> FileText;

    fn set_file_text(&mut self, file_id: vfs::FileId, text: &str);

    /// Whether [`SourceDatabase::file_text`] has the file: one that is not UTF-8 has no text.
    fn has_file_text(&self, _file_id: vfs::FileId) -> bool {
        true
    }

    fn set_file_text_with_durability(
        &mut self,
        file_id: vfs::FileId,
        text: &str,
        durability: Durability,
    );

    /// Contents of the source root.
    fn source_root(&self, id: SourceRootId) -> SourceRootInput;

    fn file_source_root(&self, id: vfs::FileId) -> FileSourceRootInput;

    fn set_file_source_root_with_durability(
        &mut self,
        id: vfs::FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    );

    /// Source root of the file.
    fn set_source_root_with_durability(
        &mut self,
        source_root_id: SourceRootId,
        source_root: Arc<SourceRoot>,
        durability: Durability,
    );

    /// See [`Files::set_appended_root_module`].
    fn set_appended_root_module(
        &mut self,
        file_id: vfs::FileId,
        module: Option<&'static str>,
        durability: Durability,
    );

    /// Sets the inputs of one view's files, writing only those that changed: see
    /// [`Files::set_view_root`].
    fn set_view_root(&mut self, view: ViewRoot<'_>) -> Result<ViewChange, ViewError>;

    fn resolve_path(&self, path: AnchoredPath<'_>) -> Option<FileId> {
        // FIXME: this *somehow* should be platform agnostic...
        let source_root = self.file_source_root(path.anchor);
        let source_root = self.source_root(source_root.source_root_id(self));
        source_root.source_root_ref(self).resolve_path(path)
    }

    /// The file `path` names relative to `anchor`, when it is not one of the files of the VFS,
    /// as `krate` reads it.
    fn included_file(&self, _krate: Crate, _path: AnchoredPath<'_>) -> Option<IncludedFile> {
        None
    }

    /// Notes that `include!` or a module declaration names a file `resolve_path` has nothing
    /// for: a file that is not source by its extension, or one outside its package, which the
    /// VFS does not load until it is asked to. `krate` is the crate whose expansion or module
    /// tree names it.
    fn include_source(&self, _path: AnchoredPath<'_>, _krate: Crate) {}

    /// Notes that `env!` or `option_env!` asked for the variable `key` in `krate`.
    fn env_is_read(&self, _krate: Crate, _key: &str) {}

    /// Notes that `env!` in `krate` found the variable `key` unset, which rustc turns the
    /// crate down for.
    fn env_is_unset(&self, _krate: Crate, _key: &str) {}

    #[doc(hidden)]
    fn crates_map(&self) -> Arc<CratesMap>;

    /// Runs `task(db, i)` for every `i` in `0..count`, and returns once each has run: the
    /// tasks of a round of macro expansion, or the def maps of a crate's dependencies. Each
    /// task asks queries and nothing else, so neither the order nor the threads they run on
    /// changes a result, and the caller reads what they computed afterwards in its own order.
    ///
    /// The database runs them as plane tasks on the one worker pool, each reading this database
    /// by reference, and the calling thread pump-waits for them (docs/v2/plan/lanes/db.md, "One
    /// database, shared by every thread"; #1301): no handle per thread, no thread of its own.
    fn run_all(&self, count: usize, task: &(dyn Fn(&dyn SourceDatabase, usize) + Sync));

    fn nonce_and_revision(&self) -> (Nonce, salsa::Revision);

    fn line_column(&self, file: FileId, offset: TextSize) -> Result<(u32, u32), ()>;
}

/// Tells salsa which plane task a thread runs, from the pool's pump
/// ([`blaze_graph::pump::depth`]), so a task that would wait on one below it on its own thread
/// unwinds with [`salsa::Cancelled::WouldWaitOnItself`] (which [`run_on_pool`] runs again)
/// instead of waiting forever. For each database whose [`SourceDatabase::run_all`] is
/// [`run_on_pool`]; once per process, so a second call does nothing.
pub fn install_plane_hooks() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| salsa::set_task_depth(blaze_graph::pump::depth));
}

/// [`SourceDatabase::run_all`] on `pool`: each task is a job near this thread's group, reading
/// `db` by reference, and the calling thread pump-waits for the batch ([`Pool::scope`]).
///
/// A task that unwinds with [`salsa::Cancelled::WouldWaitOnItself`] asked for a query that a
/// task below it on its thread runs, which can't end before it does; it runs again in another
/// round, once every task of this one has ended. A job's completion can't reach its scope, so
/// the round is the way back. The first other panic, salsa's other cancellations included,
/// goes on from here once its round is over, and nothing is run again after it.
///
/// [`Pool::scope`]: blaze_graph::pool::Pool::scope
pub fn run_on_pool(
    db: &dyn SourceDatabase,
    pool: &blaze_graph::pool::Pool,
    count: usize,
    task: &(dyn Fn(&dyn SourceDatabase, usize) + Sync),
) {
    let near = blaze_graph::pool::group();
    let panicked = std::sync::Mutex::new(None);
    let again = std::sync::Mutex::new(Vec::new());
    let mut round: Vec<usize> = (0..count).collect();
    while !round.is_empty() {
        pool.scope(|scope| {
            for &at in &round {
                let (panicked, again) = (&panicked, &again);
                scope.submit(
                    near,
                    move || task(db, at),
                    move |ran| {
                        let Err(panic) = ran else { return };
                        let waited_on_itself = matches!(
                            panic.downcast_ref::<salsa::Cancelled>(),
                            Some(salsa::Cancelled::WouldWaitOnItself)
                        );
                        if waited_on_itself {
                            again.lock().unwrap_or_else(|it| it.into_inner()).push(at);
                        } else {
                            let mut first = panicked.lock().unwrap_or_else(|it| it.into_inner());
                            first.get_or_insert(panic);
                        }
                    },
                );
            }
        });
        round = std::mem::take(&mut *again.lock().unwrap_or_else(|it| it.into_inner()));
        if panicked.lock().unwrap_or_else(|it| it.into_inner()).is_some() {
            break;
        }
    }
    if let Some(panic) = panicked.into_inner().unwrap_or_else(|it| it.into_inner()) {
        panic::resume_unwind(panic);
    }
}

static NEXT_NONCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nonce(usize);

impl Default for Nonce {
    #[inline]
    fn default() -> Self {
        Nonce::new()
    }
}

impl Nonce {
    #[inline]
    pub const fn invalid() -> Nonce {
        Nonce(usize::MAX)
    }

    #[inline]
    pub fn new() -> Nonce {
        Nonce(NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
    }
}

/// Crate related data shared by the whole workspace.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct CrateWorkspaceData {
    pub target: Result<target::TargetData, target::TargetLoadError>,
    /// Toolchain version used to compile the crate.
    pub toolchain: Option<Version>,
    /// How far the evaluation of one constant of the workspace may go.
    pub const_eval: ConstEvalBudget,
}

/// What one evaluation of a constant, static or other compile-time code may spend before it
/// stops with an error: `const-eval` under `[workspace.metadata.blaze]` in the root manifest.
///
/// ```toml
/// [workspace.metadata.blaze]
/// const-eval = { steps = 4_000_000_000, memory = 2_147_483_648 }
/// ```
///
/// rustc's evaluation has no limit of its own past its `long_running_const_eval` lint (at 2
/// million steps, allowed away by `#[allow]`), so the default is far beyond what code rustc
/// takes without that `#[allow]` ever spends: it is there to end a loop that never would.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct ConstEvalBudget {
    /// Basic blocks run, as rustc counts the terminators it evaluates.
    pub steps: u64,
    /// Bytes of the evaluation's stack and heap together.
    pub memory: u64,
}

impl ConstEvalBudget {
    /// The key of the table under `[workspace.metadata.blaze]`.
    pub const KEY: &str = "const-eval";
}

impl Default for ConstEvalBudget {
    fn default() -> Self {
        ConstEvalBudget { steps: 1_000_000_000, memory: 1 << 30 }
    }
}

impl CrateWorkspaceData {
    /// Of the target ([`target::TargetData::content`], or 1 and why it could not be loaded),
    /// the toolchain's version, and the const-eval budget.
    pub const DOMAIN: &'static str = "blaze v2 crate workspace data 1";

    pub fn content(&self) -> ContentId {
        let mut hasher = Hasher::new(CrateWorkspaceData::DOMAIN);
        match &self.target {
            Ok(target) => hasher.update(&[0]).id(&target.content()),
            Err(error) => hasher.update(&[1]).part(error.to_string().as_bytes()),
        };
        content::opt(&mut hasher, self.toolchain.as_ref().map(Version::to_string).as_deref());
        hasher
            .update(&self.const_eval.steps.to_le_bytes())
            .update(&self.const_eval.memory.to_le_bytes())
            .finish()
    }

    pub fn is_atleast_187(&self) -> bool {
        const VERSION_187: Version = Version {
            major: 1,
            minor: 87,
            patch: 0,
            pre: Prerelease::EMPTY,
            build: BuildMetadata::EMPTY,
        };
        self.toolchain.as_ref().map_or(false, |v| *v >= VERSION_187)
    }
}

pub fn toolchain_channel(db: &dyn salsa::Database, krate: Crate) -> Option<ReleaseChannel> {
    krate.workspace_data(db).toolchain.as_ref().and_then(|v| ReleaseChannel::from_str(&v.pre).ok())
}

#[salsa::input(singleton, debug)]
struct AllCrates {
    #[returns(clone)]
    crates: std::sync::Arc<[Crate]>,
    #[returns(copy)]
    content: InputId,
}

/// The crate graph's content ID is of its crates' content IDs, in topological order.
pub const CRATE_GRAPH_DOMAIN: &str = "blaze v2 crate graph 1";

fn crate_graph_content(db: &dyn salsa::Database, crates: &[Crate]) -> ContentId {
    let mut hasher = Hasher::new(CRATE_GRAPH_DOMAIN);
    content::len(&mut hasher, crates.len());
    for krate in crates {
        hasher.id(&krate.content(db));
    }
    hasher.finish()
}

/// The content ID of [`all_crates`].
pub fn all_crates_content(db: &dyn salsa::Database) -> ContentId {
    match AllCrates::try_get(db) {
        Some(all) => all.content(db).id(),
        None => crate_graph_content(db, &[]),
    }
}

/// Sets the crate graph's content ID from its crates' as they are.
pub(crate) fn refresh_crate_graph_content(db: &mut dyn salsa::Database) {
    let Some(all) = AllCrates::try_get(&*db) else { return };
    let content = InputId(crate_graph_content(&*db, &all.crates(&*db)));
    if all.content(&*db) != content {
        all.set_content(db).to(content);
    }
}

pub fn set_all_crates_with_durability(
    db: &mut dyn salsa::Database,
    crates: impl IntoIterator<Item = Crate>,
    durability: Durability,
) {
    let crates: std::sync::Arc<[Crate]> = crates.into_iter().collect();
    let content = InputId(crate_graph_content(&*db, &crates));
    match AllCrates::try_get(&*db) {
        Some(all) => {
            all.set_crates(db).with_durability(durability).to(crates);
            all.set_content(db).with_durability(durability).to(content);
        }
        None => {
            _ = AllCrates::builder(crates, content).durability(durability).new(&*db);
        }
    }
}

/// Returns the crates in topological order.
///
/// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
pub fn all_crates(db: &dyn salsa::Database) -> std::sync::Arc<[Crate]> {
    AllCrates::try_get(db).map_or(std::sync::Arc::default(), |all_crates| all_crates.crates(db))
}

// FIXME: VFS rewrite should allow us to get rid of this wrapper
#[doc(hidden)]
#[salsa::interned]
pub struct InternedSourceRootId {
    #[returns(copy)]
    pub id: SourceRootId,
}

/// Crates whose root file is in `id`.
pub fn source_root_crates(db: &dyn SourceDatabase, id: SourceRootId) -> &[Crate] {
    #[salsa::tracked(returns(deref))]
    pub fn source_root_crates<'db>(
        db: &'db dyn SourceDatabase,
        id: InternedSourceRootId<'db>,
    ) -> Box<[Crate]> {
        let crates = AllCrates::get(db).crates(db);
        let id = id.id(db);
        crates
            .iter()
            .copied()
            .filter(|&krate| {
                let root_file = krate.data(db).root_file_id;
                db.file_source_root(root_file).source_root_id(db) == id
            })
            .collect()
    }
    source_root_crates(db, InternedSourceRootId::new(db, id))
}

pub fn relevant_crates(db: &dyn SourceDatabase, file_id: FileId) -> &[Crate] {
    let _p = tracing::info_span!("relevant_crates").entered();

    let source_root = db.file_source_root(file_id);
    source_root_crates(db, source_root.source_root_id(db))
}

#[must_use]
#[non_exhaustive]
pub struct DbPanicContext;

impl Drop for DbPanicContext {
    fn drop(&mut self) {
        Self::with_ctx(|ctx| assert!(ctx.pop().is_some()));
    }
}

impl DbPanicContext {
    pub fn enter(frame: String) -> DbPanicContext {
        #[expect(clippy::print_stderr, reason = "already panicking anyway")]
        fn set_hook() {
            let default_hook = panic::take_hook();
            panic::set_hook(Box::new(move |panic_info| {
                default_hook(panic_info);
                if let Some(backtrace) = salsa::Backtrace::capture() {
                    eprintln!("{backtrace:#}");
                }
                DbPanicContext::with_ctx(|ctx| {
                    if !ctx.is_empty() {
                        eprintln!("additional context:");
                        for (idx, frame) in ctx.iter().enumerate() {
                            eprintln!("{idx:>4}: {frame}\n");
                        }
                    }
                });
            }));
        }

        static SET_HOOK: Once = Once::new();
        SET_HOOK.call_once(set_hook);

        Self::with_ctx(|ctx| ctx.push(frame));
        DbPanicContext
    }

    fn with_ctx(f: impl FnOnce(&mut Vec<String>)) {
        thread_local! {
            static CTX: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        }
        CTX.with(|ctx| f(&mut ctx.borrow_mut()));
    }
}
