# Requests of lane cargo

Lane cargo's design is `docs/v2/plan/lanes/cargo/notes.md`. Its children (cargo-load, cargo-std,
cargo-daemon, cargo-cli) append below, each request saying which child it comes from.

## 1. Ownership: blaze-manifest, blaze-metadata, blaze-resolve, blaze-registry, blaze-source to lane cargo

What: in `docs/v2/plan/reference/crates.md`, add `crates/blaze-manifest/`, `crates/blaze-metadata/`,
`crates/blaze-resolve/`, `crates/blaze-registry/` and `crates/blaze-source/` to lane cargo's
paths, and take `blaze-tree` and `blaze-units` out of the "Every other path" paragraph (the table
already gives them to cargo; the paragraph says they are the coordinator's).

Why: `docs/v2/plan/process/building.md` section 18 gives cargo "everything cargo-shaped that no lane had", and the workspace
load as queries over a view can't be written without changing these crates. Today they read the
machine's disk directly: `blaze_manifest::manifest::read_manifest` (`std::fs::read`), target
autodiscovery (`targets.rs`: `exists`, `is_file`, `read_dir`), workspace member globs
(`manifest.rs`: `glob::glob`, `is_dir`), config discovery (`config/mod.rs`:
`read_to_string`, `exists`), blaze-metadata's reads of registry packages, and blaze-registry's
fetches. A query must read every file through `blaze_query::Cx` (so each read is an edge and
the result is keyed by what it read, docs/v2/plan.md section 2), and a view is a Merkle root,
not the disk (a what-if or a PR view has no directory). The change is one trait threaded
through `blaze_manifest::Context`:

```rust
// crates/blaze-manifest/src/files.rs
/// Every file the manifest, workspace and config readers look at, by absolute path as cargo
/// names it. `Disk` reads the machine's (today's behavior); the workspace-load queries
/// (crates/blaze-units/src/load/) read a view's through `blaze_query::Cx`.
pub trait Files {
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    /// `None` for nothing there.
    fn kind(&self, path: &Path) -> Option<FileKind>;
    /// The names in a directory, sorted, each with whether it is a directory.
    fn list(&self, path: &Path) -> std::io::Result<Vec<(String, bool)>>;
}
pub enum FileKind { File, Dir }
pub struct Disk;
```

with `Context::with_files(config, files: Rc<dyn Files>)` and glob expansion done over `list`
(cargo's `glob` semantics kept). blaze-metadata's registry reads take the same `Files`; fetching
moves to the daemon (effects), which mounts each fetched package as a VFS root
(`docs/v2/plan/lanes/cargo/notes.md`, "Sources"). No behavior changes for a `Disk` context, which every
v1 caller keeps; `harness/resolve.py` still holds them to cargo. Lane cargo writes all of it once
granted; until then its child cargo-load writes everything else.

> forwarded to coordinator: the grant changes `docs/v2/plan/reference/crates.md` and five crates that are the coordinator's, neither among the paths `requests` applies; building.md section 18 ("everything cargo-shaped that no lane had") supports it.

## 2. `crates/blaze-query/src/lib.rs`: a kind range for cargo, and reads of an immutable root

```rust
pub mod kinds {
    /// The workspace load and the unit graph (lane cargo): `0x6000..0x6800`.
    pub const CARGO: std::ops::Range<u16> = 0x6000..0x6800;
}

impl<'db> Cx<'db> {
    /// The file at `path` under `root`, a root of the database's tree other than the view's
    /// (a registry package's source, the standard library's `library/`): its content ID and
    /// bytes, `None` for none. A root never changes, so this is a pure function of
    /// `(root, path)`; recorded as an edge keyed by both, whose fingerprint is the file's ID.
    pub fn file_at(&mut self, root: ContentId, path: &str) -> Result<Option<blaze_vfs::File<'db>>, Cancelled>;
    /// The names in a directory under `root`, as [`Cx::dir`].
    pub fn dir_at(&mut self, root: ContentId, path: &str) -> Result<Option<&'db [(String, bool)]>, Cancelled>;
}
```

Why: a build reads files of more than the view: every registry and git dependency's source
and the standard library's are content-addressed roots of their own, mounted where cargo would
have them (`$CARGO_HOME/registry/src/...`, the sysroot's `library/`). Keyed by the root's
content ID, their results are shared by every view and project that uses the same version: the
shared base layer (docs/v2/plan/lanes/ci.md). Reading them through `Cx::file` would need every view to
contain every dependency's files. `Tree::file(root, path)` already reads any root; this only
exposes it to a query, with an edge, so `Why` can still name the file.

> forwarded to db

## 3. `crates/blaze-api/src/lib.rs`: what cargo's command line and cargo's output need (schema 5)

Why: the CLI (`blaze build|run|test|check`, `cargo blaze`) is a client of the agent API with
cargo's flags, output and exit codes unchanged (`docs/v2/plan/process/building.md` section 18). `Targets` holds six of
cargo's flags; a build also depends on target selection, `--workspace`, `--exclude`,
`--config`, `-Z`, `--manifest-path`, `--locked`/`--offline`/`--frozen`, the environment cargo
and build scripts read (`RUSTFLAGS`, `CARGO_*`, `PATH` for a script's `cc`), and config files
outside the view. And cargo's output needs per-unit progress lines, `build-script-executed`,
`compiler-artifact` and `compiler-message` JSON, and rustc's rendered diagnostics. Lane cargo's
daemon and CLI are written against exactly this.

```rust
pub const SCHEMA: u32 = 5;

pub struct Targets {
    // ... the six fields as they are, then:
    /// Everything a cargo command line says beyond the fields above, for `blaze build`,
    /// `run`, `test`, `check` and `cargo blaze`; `None` for an agent's request, built as cargo
    /// builds the fields above with its defaults. Where both say something, this wins.
    pub cargo: Option<Cargo>,
}

/// A cargo command line, as the CLI was run.
pub struct Cargo {
    /// `build`, `check`, `test`, `bench` or `run`.
    pub command: String,
    /// The arguments after the subcommand and before `--`, as typed.
    pub args: Vec<String>,
    /// Where the view's root is on the client's machine, absolute: what cargo's paths
    /// (`CARGO_MANIFEST_DIR`, `file!()`, the target directory) are under.
    pub root: String,
    /// The directory the command ran in, absolute.
    pub cwd: String,
    /// The command's environment, sorted by name. Held in memory for the request only: no
    /// query value, log or store entry holds a variable's value unless cargo reads it by name.
    pub env: Vec<(String, String)>,
    /// The files outside the view cargo reads, by absolute path: `$CARGO_HOME/config.toml`,
    /// the `.cargo/config.toml` of each directory above the root, a `--config` file outside
    /// it. Never credentials.
    pub files: Vec<(String, Vec<u8>)>,
    /// Whether diagnostics are rendered with colors (cargo's `--color`, resolved by the
    /// client against its terminal).
    pub color: bool,
}

pub enum Response {
    // ... every variant as it is, `Built` changed, and:
    /// A build's output, placed. `profile` as cargo's `Finished` line names it:
    /// "`dev` profile [unoptimized + debuginfo]".
    Built { artifacts: Vec<Artifact>, profile: String },
    /// A unit began: cargo's `Compiling foo v0.1.0 (/path)` line.
    Unit(UnitStarted),
    /// A build script ran: cargo's `build-script-executed` message.
    BuildScript(BuildScriptRan),
}

pub struct UnitStarted {
    pub package_id: String,
    pub name: String,
    pub version: String,
    /// What cargo prints after the version: `(/abs/path)` for a path package,
    /// `(https://github.com/o/r#0123abcd)` for a git one, none for a registry's.
    pub source: Option<String>,
    pub verb: Verb,
}

pub enum Verb { Compiling, Checking, Fresh }

pub struct BuildScriptRan {
    pub package_id: String,
    pub linked_libs: Vec<String>,
    pub linked_paths: Vec<String>,
    pub cfgs: Vec<String>,
    pub env: Vec<(String, String)>,
    pub out_dir: String,
}

/// A unit's target as cargo's JSON messages name it.
pub struct UnitTarget {
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub name: String,
    pub src_path: String,
    pub edition: String,
    pub doc: bool,
    pub doctest: bool,
    pub test: bool,
}

pub struct UnitProfile {
    pub opt_level: String,
    pub debuginfo: u32,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub test: bool,
}

pub struct Artifact {
    /// cargo's package id (`path+file:///w/a#0.1.0`).
    pub package: String,
    /// `bin`, `lib`, `test`, `example`, `bench`, `proc-macro`, `custom-build`, `doctest`.
    pub kind: String,
    /// The main file placed (the executable for one that runs), and its content.
    pub path: String,
    pub content: ContentId,
    pub manifest_path: String,
    pub target: UnitTarget,
    pub profile: UnitProfile,
    pub features: Vec<String>,
    /// Every file placed, as cargo's `filenames`.
    pub filenames: Vec<String>,
    pub executable: Option<String>,
    pub fresh: bool,
}

pub struct Diagnostic {
    // ... the fields as they are (with schema 4's `facts`), then:
    /// rustc's text for it, as rustc prints it (in color when the request's `Cargo::color`).
    pub rendered: String,
    /// The unit it is of, for cargo's `compiler-message` and its "generated N warnings".
    pub unit: Option<UnitOf>,
}

pub struct UnitOf {
    pub package_id: String,
    pub manifest_path: String,
    pub target: UnitTarget,
}

pub struct Span {
    // ... the fields as they are, then rustc's JSON's:
    pub byte_start: u32,
    pub byte_end: u32,
    pub primary: bool,
    pub replacement: Option<String>,
    /// `MachineApplicable`, `MaybeIncorrect`, `HasPlaceholders`, `Unspecified`.
    pub applicability: Option<String>,
}
```

The diagnostics lane fills `rendered`, the `Span` fields and `unit` (lane cargo's daemon gives
it the unit).

> applied: by ci in `crates/blaze-api/src/lib.rs` (schema 5): `Targets::cargo`, `Cargo`, `Response::{Built, Unit, BuildScript}`, `UnitStarted`, `Verb`, `BuildScriptRan`, `UnitTarget`, `UnitProfile`, the cargo-shaped `Artifact`, `Diagnostic::{rendered, unit}`, `UnitOf` and rustc's `Span` fields, as asked.

## 4. Coordinator: the `blaze` program's entry (at integration)

`crates/blaze-engine/src/main.rs` (codegen's) runs `blaze_engine::main`, which runs v1's
`blaze_cargo::cargo::command`. At integration the `blaze` program (and `cargo-blaze`, which cargo
runs for `cargo blaze`) runs `blaze_cargo::cli::main(std::env::args_os())` (lane cargo's v2
client), and `blaze serve` runs `blaze_cargo::serve::main` (lane cargo's v2 daemon).

> forwarded to coordinator: an integration step in codegen's `crates/blaze-engine/src/main.rs`; listed in the requests report for integration.

## 5. From cargo-load: request 1 also covers the config chain, and two notes

**What (addendum to request 1, `crates/blaze-manifest/src/config/mod.rs`):** the config chain reads
through the same `Files` as manifests:

```rust
impl Config {
    /// `Config::load`, every file (`.cargo/config.toml` up from `cwd`, `$CARGO_HOME/config.toml`,
    /// `include`s, a `--config` file) read through `files`; `load` is `load_with(.., Rc::new(Disk))`.
    pub fn load_with(cwd: &Path, env: impl IntoIterator<Item = (String, String)>, files: Rc<dyn Files>) -> anyhow::Result<Config>;
}
```

and `Context::with_files(config, files)` is given the same `files`. cargo-load's
`crates/blaze-units/src/load/` calls exactly `blaze_manifest::files::{Files, FileKind}`,
`Config::load_with`, `Config::start`, `Context::with_files`, `read_manifest`, `Workspace::new`.
cargo-load's `Files` are owned values (a snapshot of what the query read, filled between runs),
so `Rc<dyn Files>` with its implied `'static` is fine. When request 1 is granted, cargo-load
writes all of it.

**Note (crates/blaze-query/src/lib.rs, coordinator):** `Database::prepare` and `run_query`
encode and decode `Q::Key` with CBOR, but `Query::Key` is bounded only by `Clone + Send + Sync`;
it needs `serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static` to compile.
cargo-load's keys are all `String`, `()` or serde structs.

**Note (crates/blaze-units/src/query.rs, lane cargo; for cargo-cli):** cargo-load writes
`LoadError::what` without the leading `error: `: the message, then for each cause
`\n\nCaused by:\n  <cause>` (each line of a cause indented two spaces), as cargo prints it after
`error: `. `NUM_JOBS` of a build script's run is in `UnitPlan::env` only when `-j` or
`build.jobs` names it; otherwise the daemon sets it to its own job count, as cargo's default is the
machine's parallelism, which a view's configuration does not have.

> forwarded to coordinator: the config-chain addendum goes with request 1's grant.
> applied: the `Query::Key` note: `Query::Key` is already bounded by `Clone + Send + Sync + Serialize + DeserializeOwned + 'static` (`crates/blaze-query/src/lib.rs`).
> applied: the `LoadError::what` and `NUM_JOBS` note is lane cargo's own (no change outside its paths).

## 6. From cargo-std: the query `StdUnits` loads `library/` through, and what `StdKey` misses

**What (to cargo-load, `crates/blaze-units/src/load/`):** one query of yours that loads, resolves
and plans a workspace described wholly by its key, never by the view, so `StdUnits` goes through
the same manifest, config, lock and resolve queries a user's workspace does (cargo-notes section
5) and its results are shared by every view:

```rust
// crate::load
/// The units of a workspace other than the view's, by everything that decides them: the
/// standard library's (crate::stdlib). Reads nothing of the view and never `Cx::config`.
pub struct Foreign; // impl Query: Key = ForeignAsk, Value = Cbor<crate::query::StdPlanned>, kind yours

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ForeignAsk {
    /// Every file it reads is under these (the library's mount alone); a path under none fails
    /// naming it (nothing is fetched: library/ vendors its registry packages).
    pub mounts: Vec<Mount>,
    /// The workspace's root manifest, absolute: `<library>/Cargo.toml`.
    pub manifest: String,
    /// The config chain: `.cargo/config.toml` of `cwd` and each directory above it (library/'s
    /// replaces crates.io with `vendor/`); no `$CARGO_HOME`, no environment, no `--config`.
    pub cwd: String,
    /// `--locked`: `Cargo.lock` as it is; one the manifests disagree with is cargo's error.
    pub locked: bool,
    /// cargo's `resolve_ws_with_opts` of these specs (`-p` names), with `features` as
    /// `--features` says them, `--no-default-features`, `HasDevUnits::No`, and
    /// `require_optional_deps = false` (cargo's `resolve_std`).
    pub specs: Vec<String>,
    pub features: Vec<String>,
    /// The packages whose `lib` units are the graph's roots, by name; `StdGraph::roots` names
    /// each with its unit's index, in this order.
    pub roots: Vec<String>,
    /// What every unit but build scripts' is built for (`CompileKind::Target(triple)`, so
    /// `Unit::platform` is `Some(triple)`), with its cfg (crate::stdlib::target::target_cfg).
    pub triple: String,
    pub cfg: Vec<String>,
    /// The rustflags of every unit for `triple`, as cargo passes them (no config's are read);
    /// host units have none.
    pub rustflags: Vec<String>,
    /// What build scripts are compiled for and run on, with its cfg.
    pub host: String,
    pub host_cfg: Vec<String>,
    /// The profile of every unit: the workspace's own `[profile.<root>]` (blaze-manifest's
    /// `Profiles` of the root manifest, `root` being `dev` or `release`) with its top-level
    /// settings replaced by `ForeignProfile`'s, then its `package.*` and `build-override` tables applied
    /// as cargo applies them (library/Cargo.toml's `compiler_builtins` codegen-units, the
    /// backtrace crates' `debug = 0`, `panic_abort`'s `rustflags`). `Profile::name` is `name`.
    pub profile: ForeignProfile,
    /// Packages whose units are built with `panic = "abort"` whatever the profile says
    /// (`panic_abort`, as library/Cargo.toml's comment says it must be).
    pub panic_abort: Vec<String>,
    /// `Unit::is_std` of every unit, and what cargo does for one (`-Z
    /// force-unstable-if-unmarked`, `RUSTC_BOOTSTRAP=1`, its `-C metadata`).
    pub is_std: bool,
    /// Stands for the build's `target/[<triple>/]<profile dir>` in every path of the plans
    /// (`outputs`, `out_dir`, `OUT_DIR` in `env`): `crate::stdlib::STD_OUT`. The merge puts
    /// the build's directory in its place.
    pub out: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ForeignProfile {
    /// The build's profile, as each unit's `Profile::name` says it.
    pub name: String,
    /// `dev` or `release`: the workspace's table the settings go on.
    pub root: String,
    /// `opt-level`, `debug-assertions`, `overflow-checks`, `debug`, `panic` as the user's profile
    /// has them (`StdKey`'s).
    pub opt_level: String,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub debuginfo: String,
    pub panic: String,
}
```

`StdUnits` calls `cx.get::<crate::load::Foreign>(&ask)` and checks and returns what it says
(roots by name, every unit `is_std`). Build scripts' units are host units with no edge to std
roots, as in cargo's build-std; how one links a std is the daemon's (blaze-wasi's std, as v1).

**What (to lane cargo, `crates/blaze-units/src/query.rs`'s `StdKey`):** three inputs std's units
depend on that the key does not have, so a cache key would miss them (AGENTS.md). cargo-std's
`stdlib.rs` is written against them, as cargo-load writes against requests 1 and 2:

```rust
pub struct StdKey {
    // ... the fields as they are, then:
    /// The machine build scripts are compiled for and run on (`BuildConfig::host`): std's
    /// build scripts' units are host units, and cargo hashes the host into their metadata.
    pub host: String,
    /// The target's rustflags as cargo passes them to every unit of the platform, std's too
    /// (`RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `[build]`, `[target.<triple>]`): their `-C
    /// target-feature`, `-C target-cpu`, `--cfg` decide the cfg std is resolved and built with.
    pub rustflags: Vec<String>,
    /// `dev` or `release`: what the build's profile inherits from at the root, whose table of
    /// library/Cargo.toml (`[profile.<root>.package.*]`: `compiler_builtins`' codegen-units, the
    /// backtrace crates' `debug = 0`, `panic_abort`'s `-Cpanic=abort`) applies to std.
    pub profile_root: String,
}
```

The user's `[profile.<name>.build-override]` is still not in it: std's build scripts take
library/Cargo.toml's (none, so cargo's default: opt-level 0, no debuginfo). Say if it should be.
`query.rs`'s test `std_keys_differ_by_what_they_ask` builds a `StdKey` and needs the three
fields too.

**What (to cargo-load, `crates/blaze-units/Cargo.toml`):** `blaze-vfs.workspace = true` under
`[dev-dependencies]`: `stdlib/tests.rs` opens views (`blaze_vfs::View`) over a `MemTree`.

**What (coordinator, `harness/answers.py`):** `--write` writes `crates/blaze-units/src/stdlib/rustc.json`
too (a copy of `crates/blaze-cargo/src/answers/rustc.json`, which v2's target table reads), until
integration deletes v1's.

> forwarded to cargo: `crate::load::Foreign` and `StdKey`'s three fields are in lane cargo's own paths (`crates/blaze-units/`); write them there.
> forwarded to coordinator: `harness/answers.py` is the coordinator's (harness/ but harness/v2/), not among the paths `requests` applies.

## 7. From cargo-daemon-fetch (to cargo-daemon): `blaze-source` in `crates/blaze-cargo/Cargo.toml`

**What:** `blaze-source.workspace = true` under `[dependencies]` of `crates/blaze-cargo/Cargo.toml`
(the workspace already has `blaze-source = { path = "crates/blaze-source" }`; blaze-registry and
blaze-units depend on it).

**Why:** `serve/fetch/git.rs` fetches a git tree through `blaze_registry::git::fetch(&Context,
SourceId, Some(commit))`, which takes a `blaze_source::SourceId` (`SourceId::for_git(&url,
GitReference::Rev(commit))`), and checks that the fetch's `at` is
`$CARGO_HOME/git/checkouts/<SourceId::git_ident()>/<short id>`. Without it `serve/fetch/git.rs`
does not compile.

**Note (to cargo-load-source):** `Fetch` carries no credential, so a `.crate` or index file of a
registry that requires authentication (`auth-required`) answers 401 and fails with cargo's HTTP
error. If v2 is to fetch from such registries, `Fetch` needs the token (never logged), or the
daemon an `Auth`; that is an interface change to `crates/blaze-units/src/query.rs`.

> forwarded to cargo: between two of lane cargo's children, in its own `crates/blaze-cargo/Cargo.toml` (where `blaze-source.workspace = true` is now, line 72); the auth note concerns `crates/blaze-units/src/query.rs`, also cargo's.

## 8. From cargo-std (to lane cargo and cargo-load): request 6 is still unwritten, and is all cargo-std waits on

`crates/blaze-units/src/stdlib.rs` and `stdlib/tests.rs` are finished against request 6. As of
2026-10-29 17:40 none of it is in the tree, so `crate::stdlib` does not compile with feature
`query`:

- **lane cargo, `crates/blaze-units/src/query.rs`:** `StdKey::{host, rustflags, profile_root}`
  as request 6 gives them (and `std_keys_differ_by_what_they_ask` setting them). `stdlib.rs`'s
  `foreign_ask` reads all three; `stdlib/tests.rs`'s `key()` sets them.
- **cargo-load, `crates/blaze-units/src/load.rs`:** `pub struct Foreign` (`impl Query`, `Key =
  ForeignAsk`, `Value = Cbor<StdPlanned>`, a kind of `kinds::LOAD`), `pub struct ForeignAsk`
  and `pub struct ForeignProfile` with exactly request 6's fields, and `crate::load::register`
  registering `Foreign`. `StdUnits::execute` calls `cx.get::<crate::load::Foreign>(&ask)?.get()`.
  `Foreign` reads library/ through `Cx::file_at` (request 2), so it waits on that too.
- **cargo-load, `crates/blaze-units/Cargo.toml`:** `blaze-vfs.workspace = true` under
  `[dev-dependencies]` (`stdlib/tests.rs` opens views with `blaze_vfs::View`, which blaze-query
  does not re-export).

If lane cargo would rather cargo-std write `Foreign` itself, grant `crates/blaze-units/src/load/foreign.rs`
to cargo-std and say which load queries (manifest snapshot, lock, resolve, units, `plan`) it
calls; until then cargo-std writes nothing in `load/`.

> applied by lane cargo (requests 6 and 8): `StdKey::{host, rustflags, profile_root}` are in
> `crates/blaze-units/src/query.rs` as request 6 gives them (key domain bumped to `std key 2`;
> the test sets them). `crate::load::Foreign`, `ForeignAsk`, `ForeignProfile` with request 6's
> fields, and `blaze-vfs` under `[dev-dependencies]`, go to cargo-load: `Foreign` is the same
> manifest, config, lock, resolve and units pipeline as `Plan`, its inputs taken from the key
> instead of `Cx::config`, so the child writing that pipeline writes it (in `load.rs` or
> `load/foreign.rs`, its choice). The user's `build-override` stays out of `StdKey`: std's build
> scripts take library/Cargo.toml's, as `-Zbuild-std` does.

## 9. From cargo-std: two steps to the speed bar that are outside cargo-std's paths

- **coordinator, `harness/answers.py --write`:** besides `crates/blaze-units/src/stdlib/rustc.json`,
  also write `crates/blaze-units/src/stdlib/table.rs`: the per-target columns `stdlib/target.rs`
  compiles at its first call, as `static` data. Then no JSON is parsed at run time (today about
  1 ms once per process). cargo-std will switch `target.rs` to that file once its format is agreed.
- **lane cargo, `crates/blaze-units/src/query.rs`:** a `Persist` for already-encoded CBOR, with
  the same schema and layout as `Cbor<T>` (`CborBytes<'a, T>(&'a [u8])`), so `StdUnits` can
  return `Foreign`'s bytes after checking them instead of decoding and encoding them again.

> forwarded to coordinator: `harness/answers.py` writing `stdlib/table.rs` is the coordinator's, not among the paths `requests` applies.
> forwarded to cargo: `CborBytes<'a, T>` in `crates/blaze-units/src/query.rs` is lane cargo's own path; lane cargo is still running.

## From concurrency: lock audit (2026-10-29)

Read-only audit of `crates/blaze-cargo/` (not `remote*`, not `sandbox*`), `crates/load-cargo/`,
`crates/project-model/` against docs/v2/plan/design/concurrency.md, by concurrency-audit-build.
v1's daemon and CLI (`cargo/daemon.rs`, `compile_time.rs`, `trace.rs`, `events.rs`, ...) are
left as they are: `serve/` calls none of them. The findings are in v2's daemon, `serve/`, and in
`load-cargo/src/v2.rs`, which it calls once per unit. `serve/unit/{compile,codegen,script,test,
run,reach}.rs` are not written yet, so the locks below are judged by what their fields' docs and
`work.rs`'s contract say runs where. Each can be dropped before those modules use it.

1. **`crates/blaze-cargo/src/serve/work.rs:75`: `JobInput::store: Arc<RwLock<Store>>`.** Every
   worker task (`lower`, `compile`, `reach`) reads the store through it, so each read takes the
   `RwLock`'s read side, a write to one shared counter by every core. That breaks "a read writes
   no shared memory". It also makes every reader wait whenever the loop takes the write side to
   place `Compiled::puts`. **Instead:** `pub store: blaze_cas::Reader` (lane concurrency is
   writing it: lock-free reads of the published index), with puts going to the store's owner as
   they already do through `Compiled::puts`. `Runs::start(&Store)` and `Runs::complete(&mut
   Store)` are on the loop, which owns the `Store` outright, so they need no lock.
2. **`crates/blaze-cargo/src/serve/unit.rs:68`: `Shared::disk: Mutex<blaze_usercode::inputs::Disk>`.**
   `Disk` is a stat-and-hash cache whose `Inputs::file(&mut self)` hashes whole files
   (`blaze-usercode/src/inputs.rs:124`). So this one daemon-wide mutex is held across blocking
   file reads and BLAKE3, and every job's freshness check and run-key check queues on it. It is
   both a lock on a read path and held while computing. `Disk::set_env` on a `Disk` shared by
   every job also means two jobs with different environments overwrite each other's environment
   unless set and use stay under one guard. **Instead:** keep no shared `Disk` here. Give each
   task its own `Disk` over a shared, append-only `path hash → (Stamp, [u8; 32])` table: lookups
   are acquire loads, a changed stamp publishes a new entry with one release store, and hashing
   happens outside any lock. Pass the job's environment as an argument, never as state on the
   shared value. The `&self` table belongs in `blaze-usercode::inputs`, which usercode owns; the
   same finding is in v2/requests/usercode.md.
3. **`crates/blaze-cargo/src/serve/unit.rs:71`: `Shared::images: Mutex<HashMap<String, codegen::Linked>>`.**
   It is taken per linked unit on a worker (`Front::compile`), and as written the obvious use
   holds it across the incremental relink: a global lock on a hot write path, held while
   computing. **Instead:** each image is touched by one unit's compile at a time, so let the
   loop own the map. It hands the previous `Linked` into the compile task when it posts it, and
   takes the new one back in `Compiled`, so workers share nothing. If workers must look it up,
   use an append-only table keyed by the output path's hash, with each `Linked` published once
   and replaced by swapping the entry, never mutated in place.
4. **`crates/blaze-cargo/src/serve/unit.rs:152`: `Job::scripts: Mutex<BTreeMap<usize, script::Applied>>`.**
   The loop writes it once per build-script run, and every dependent's `compile` on a worker
   reads it (`rustc-link-lib`, `rustc-link-search`, `DEP_*`). That is a global mutex on a read
   path, and a `BTreeMap` walk per read. **Instead:** `scripts: Box<[OnceLock<script::Applied>]>`,
   indexed by unit (units are dense `0..graph.units.len()`). The loop `set`s an entry before it
   posts any dependent, and readers `get` it with one acquire load. Each script runs once per
   job, so a write-once slot fits.
5. **`crates/blaze-cargo/src/serve/unit.rs:154`: `Job::made: Mutex<BTreeMap<usize, compile::Made>>`.**
   Each unit's `compile` writes it and later units' compiles and the test start read it, so the
   same mutex sits on a read path shared by every worker. **Instead:** `made:
   Box<[OnceLock<compile::Made>]>` by unit, set once by the compile that made it (or by the loop
   from `Compiled`), read lock-free.
6. **`crates/load-cargo/src/v2.rs:107` (taken at `:358`, `:395`, `:438` via `Lowered::db`, `:446`): `Lowered::db: Mutex<Db>`.**
   - `Lowered::snapshot` locks the database to clone a salsa snapshot, once per worker task
     (per unit compiled or reached): a global mutex on a read path.
   - `Lowered::ran` (`:395`) and `Lowered::macros` (`:438`) hold the same guard across salsa
     writes (`set_files`, `set_cfg_options`, `set_env`, `ProcMacros::set_by_crate`) and reads
     (`cfg_options`, `env`, `by_crate`). Salsa's write blocks until every live snapshot has
     been cancelled and dropped. So a worker calling `snapshot()` meanwhile blocks on the
     mutex behind a loop thread that is itself waiting on other workers. That is a guard held
     across a query call (deadlock and priority-inversion risk), and a thread waiting on
     another's work.

   **Instead:** split `Lowered` into a loop-owned writer (`Db` by value, `ran(&mut self)`,
   `macros(&mut self)`, no mutex) and the read-only part every task shares (`units`, `sets`).
   The loop makes a task's snapshot when it posts the task and passes it in `JobInput` or the
   task, so a worker never touches shared state to get one. After a write, the loop makes new
   snapshots for the tasks it posts next.
7. **`crates/load-cargo/src/v2.rs:113` (taken at `:397`): `Lowered::applied: Mutex<Vec<Vec<(usize, Applied)>>>`.**
   Only the loop writes it, inside `ran`, but it is held across the same salsa reads and writes
   as finding 6 (`:413`–`:418`). **Instead:** a plain field of the loop-owned writer from finding
   6, with no lock.

Note, not a lock-rule finding: `crates/blaze-cargo/src/serve/fetch/git.rs:35` takes cargo's
`PackageCacheLock`, a file lock held across a git network fetch on an IO thread. It serializes
every git fetch the daemon makes, and makes the daemon wait on a running cargo ("Blocking waiting
for file lock on package cache"). docs/v2/plan/design/concurrency.md says "No build lock". It's
off the hot path, but check whether a fetch into a `CARGO_HOME` of blaze's own could commit by
atomic rename and drop the lock.

> forwarded to cargo: every finding is in lane cargo's own paths (`crates/blaze-cargo/src/serve/`, `crates/load-cargo/`); lane cargo is still running.

## 10. From cargo-load-source (to cargo-load and cargo-daemon-fetch)

**What (to cargo-load, `crates/blaze-units/Cargo.toml`):** `sha2 = { version = "0.10", optional = true }`
under `[dependencies]`, and `"dep:sha2"` in the `query` feature (blaze-registry and blaze-store
already take `sha2 = "0.10"`; it is in `Cargo.lock`).
**Why:** `src/load/source/tree.rs` checks a `directory` source's files against its
`.cargo-checksum.json` as cargo's `DirectorySource::verify` does (`Sha256::digest`). Without it
`load/source/tree.rs` does not compile.

**What (to cargo-daemon-fetch, `crates/blaze-cargo/src/serve/fetch/`):** a `FetchKind::Crate`
whose `url` is `file:///<path>` is a `local-registry`'s `.crate`: read it at that path (no network,
so also offline), check its sha256 and unpack it at `at`
(`$CARGO_HOME/registry/src/<dir>/<name>-<version>`), as cargo's `LocalRegistry` unpacks it.
`Sources::locate` makes such a fetch only once the `.crate` reads through a mount.
**Why:** `archive.rs` fetches over HTTP only; a local registry's packages otherwise never unpack.

**Note (to cargo-daemon-fetch, re the note of request 7):** a sparse registry whose `config.json`
has `auth-required = true` is already `Located::Failed` ("... requires authentication
(`auth-required` in its config.json), and blaze sends no registry token yet"), so no fetch of
one is made until `Fetch` carries a credential.

> forwarded to cargo: between lane cargo's children, in its own `crates/blaze-units/Cargo.toml` (`sha2` 0.10 is a workspace-less dependency there; the lock has it) and `serve/fetch/`.

## 11. From cargo-cli (to ci, `crates/blaze-api/src/lib.rs`; and to cargo-daemon, which fills them)

`crates/blaze-cargo/src/cli/` is written against these. Without them it does not compile.

```rust
pub enum Request {
    // ... every variant as it is, and:
    /// `blaze stop`: the daemon's state made durable, then it exits; answered with
    /// `Done { ok: true }` before it does.
    Stop,
}

pub struct UnitOf {
    // ... the fields as they are, and:
    /// The unit is compiled with `--test` (cargo's `CompileMode::Test`): cargo names it
    /// `` `foo` (lib test) `` in "generated N warnings" and "could not compile".
    pub test: bool,
}

pub struct Artifact {
    // ... the fields as they are, and:
    /// What runs the executable before it, cargo's `target.<triple>.runner` (and
    /// `CARGO_TARGET_<TRIPLE>_RUNNER`): program then arguments; empty for none.
    pub runner: Vec<String>,
    /// The variables cargo sets for running it (`Compilation::target_process`, `fill_env`):
    /// `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`, `CARGO_PKG_*`, `CARGO_CRATE_NAME`... ,
    /// `OUT_DIR` of a package with a build script, the library search path
    /// (`DYLD_FALLBACK_LIBRARY_PATH`/`LD_LIBRARY_PATH`/`PATH`) with cargo's directories first.
    /// None of the user's environment. Empty for an artifact that does not run.
    pub env: Vec<(String, String)>,
    /// The target's `harness` (`[[test]] harness = false` is false): cargo passes `--quiet`
    /// to a harness under `-q`.
    pub harness: bool,
}
```

**Why:** the CLI runs `run`'s program and `test`'s executables itself, on the terminal
(docs/v2/plan/lanes/cargo/notes.md section 7), and reads no manifest, so what cargo knows of
how to run each (its runner, its variables, whether it has libtest's harness) has to come
with the artifact; v1's client had it from v1's daemon (`Program::env`). `UnitOf::test`: cargo's
per-unit summary lines name a test unit differently, and `UnitTarget` cannot tell a lib's
test unit from the lib. `Stop`: cargo-daemon's `serve.rs` already documents
`blaze_api::Request::Stop`.

**To cargo-daemon:** for `Cargo::command == "run"` the daemon builds what cargo's
`commands/run.rs` and `ops::run` build: one program, with `default-run` applied and their
errors ("`cargo run` could not determine which binary to run...", "a bin target must be
available for `cargo run`") as `Response::Error`; the CLI only checks the one it got
(`cli/run.rs`'s `pick`). For `test`/`bench`, a doc test is an `Artifact` of kind `doctest`
with `executable` set. The CLI starts `blaze serve` (no arguments) with stdout and stderr
appended to `<serve::state()>/serve.log`, and waits up to 10 s for `serve::socket()`.

**Asked, not written against (output that differs from cargo's until it is there):**
- a `Response::UnitEnded(UnitOf)` when a unit's compile ends: cargo prints a unit's
  "generated N warnings" / "could not compile" when it ends; the CLI prints them all at the
  build's end.
- the workspace's member count in `Response::Built` (cargo's `needs_spec_flag`): the CLI puts
  `-p foo` in "to rerun pass" when `-p` was given or the built tests are of more than one
  path package.

> applied: lane ci is DONE. `crates/blaze-api/src/lib.rs`: `Request::Stop`, `UnitOf::test`, `Artifact::{runner, env, harness}` as written; the session answers `Stop` with `Done { ok: true }` alone (`session.rs`, test `stop_is_answered_done_at_once`). The two asked: `Response::UnitEnded(UnitOf)` (after a unit's last diagnostic) and `Response::Built { artifacts, profile, members: u32 }` (the workspace's member count); every `Response::Built { .. }` lane cargo writes now needs `members`, and the daemon sends `UnitEnded`. `SCHEMA` is 6. `blaze-ui` renders `Fetched` as cargo does and `UnitEnded` as nothing; `blaze-ci` ignores both.

## 10. From cargo-std (to cargo-load): `#![forbid(unsafe_code)]` for blaze-units (docs/v2/plan/design/unsafe.md)

No module of `crates/blaze-units` has `unsafe` (`rg unsafe crates/blaze-units` finds none). Put
`#![forbid(unsafe_code)]` at the top of `crates/blaze-units/src/lib.rs`, unless `load/` brings
an unsafe module. Until then, `stdlib.rs` forbids it for itself and `stdlib/` with a module-level
`#![forbid(unsafe_code)]`. `crates/project-model` now forbids it crate-wide: its only `unsafe`
was a test's `std::env::set_var`, which is unsound beside parallel tests, and the test now gives
those variables through `extra_env`.

> forwarded to cargo: `crates/blaze-units/src/lib.rs` is lane cargo's (cargo-load); lane cargo is still running.

## 12. From cargo-daemon-lower (to cargo-std, and to db)

`crates/load-cargo/src/v2.rs` is written against both of these; it does not compile until they
are in.

**To cargo-std, `crates/blaze-units/src/stdlib/target.rs`:** a public
`pub fn data_layout(triple: &str) -> Result<String, String>`, the `"data-layout"` of the
recorded target spec of `triple` (what `Target::spec` reads from `target_spec_json`; the error
`target(triple)` gives for a triple with no data, and one naming the triple when the spec has no
`data-layout`). Why: every crate's `CrateWorkspaceData::target` needs the target's LLVM data
layout for layout and const eval, and blaze runs no `rustc --print target-spec-json`
(no-rustc); `rustc.json` already has it, but `Spec` and `spec` are private. Add `data_layout`
to `Spec` (`#[serde(rename = "data-layout")]`) and expose it.

**To db, `crates/base-db/src/input.rs`:** `impl BuiltDependency { pub fn with_prelude(name:
CrateName, crate_id: Crate, prelude: bool, sysroot: bool) -> Self }`, as `DependencyBuilder`
has. Why: v2 makes crates with `Crate::create` (to give the standard library's crates
`Durability::HIGH`, which `CrateGraphBuilder::set_in_db` cannot), and cargo's build-std
passes the standard library's crates as `noprelude`, which `BuiltDependency::new` (always in
the extern prelude) cannot say; the fields are private.

**Known gap, to db:** a module a member reads by `#[path]` outside its package's directory
(rustc reads any path) is in no source root the crate can resolve from, since v2 gives each
package its own source root (so an edit invalidates only its package, and a build script's
`OUT_DIR` is read by its package alone). `SourceDatabase::resolve_path` would need to find a file
of the view by absolute path in another source root. The standard library, which does this
(`core` reads `library/stdarch`), is one source root and is not affected.

> forwarded to cargo: `stdlib/target.rs` `data_layout` is inside lane cargo (cargo-std). > forwarded to db: `BuiltDependency::with_prelude` (`crates/base-db/src/input.rs`) and the `#[path]` outside the package gap are lane db's.

## 13. From cargo-daemon (to ci, core, coordinator, cargo-load and cargo-cli)

`crates/blaze-cargo/src/serve/` is written against the first two; it does not compile until
they are in.

**To ci, `crates/blaze-api/src/lib.rs` and `src/session.rs`:**

```rust
pub enum Request {
    // ... as request 11 has it:
    Stop,
}

pub enum Response {
    // ... every variant as it is, and:
    /// cargo's `Updating crates.io index` / `Downloaded serde v1.0.219` as the daemon fetches
    /// what the load asked for: once for each source a job fetches, once for each index or
    /// git repository it updates.
    Fetched { verb: FetchVerb, what: String },
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum FetchVerb {
    Updating,
    Downloaded,
}
```

The session answers `Request::Stop` with `Done { ok: true }` and nothing else; the daemon sees
the `Stop` arrive, writes its image and estimates, waits for the image to be mapped, then stops
listening and exits (`serve/daemon.rs`). `Response::Fetched` is reported by a job like any other
response (`Session::report` passes it on); the CLI prints it as cargo does.
**Why:** `blaze stop` (request 11), and cargo's fetch lines, which in v2 only the daemon knows.

**Also to ci (asked, not written against):** the session opens each job's database view with
`Database::open` (no config), and keeps that view for `Request::Why`. The daemon's builds run in
views it opens itself with `Database::open_with(view, BuildConfig::encode())` (the load reads its
invocation and mounts from the config), so `Why` reads a view no build ran in. Proposed:
`Session::finish_in(cx, job, ok, error, queries: Option<ViewId>, out)`, `queries` the database
view the job built in, which the session then keeps for `Why` in place of its own (and closes
when it lets it go); the daemon hands it over instead of closing it.

**To core, `crates/blaze-core/src/{uring,kqueue,threads,iocp}.rs`:** on each driver
`pub fn scratch(self, dir: PathBuf) -> Self`: where a confined spawn's scratch directory is made
(`<dir>/<pid>-<n>`) in place of `<root>/.blaze-spawn`. **Why:** the daemon's driver root is `/`
(every path it is asked of is absolute: sockets, checkouts, the state directory, outputs), and
`/.blaze-spawn` is not writable; `serve.rs` passes `<state>/spawn`.

**To coordinator, the `blaze` binary (`crates/blaze/src/main.rs`):**
`#[global_allocator] static ALLOC: blaze_graph::arena::Counting<std::alloc::System> =
blaze_graph::arena::Counting(std::alloc::System);`. **Why:** the plane admits tasks by memory
(`arena::measure` of each task's peak feeds `Estimates::observe`; `arena::held` is the plane's
`Held` and `Status::memory_bytes`); without the counting allocator both read 0.

**Notes to cargo-load (`crates/blaze-units/src/load*`):**
- An `IndexFile` fetch's `what` is what cargo prints after `Updating`: the registry's name
  (`crates.io index`); the daemon reports each distinct `what` once per job.
- The daemon never decides it is offline: `--offline`/`--frozen`/`net.offline` with a source
  missing is the load's `Planned::Failed` with cargo's error. A `Needs` that names a source
  already mounted for that view is refused as a loop ("asked for ... again after it was fetched
  and mounted").
- The view is mounted at `invocation.root` (the checkout's path for the CLI, else
  `<state>/views/<root hex>` for an agent's view), every fetched source at its `Fetch::at`, the
  standard library at `BuildConfig::library`; mounts are sorted by `at`.

**To cargo-cli:** `blaze_cargo::serve::{socket, state, main}` are as request 11 uses them.
`blaze serve` takes `--socket PATH` and `--http ADDR` (token in `BLAZE_API_TOKEN`). A second
`blaze serve` on a socket something listens on fails naming it; a stale socket file is removed.

**To cargo-daemon-units, `serve/unit.rs`:** `load_cargo::v2::Lower` has one more field,
`host_cpu: Option<&HostCpu>` (from `hir_ty_codegen::host_cpu()`); without it a
`-C target-cpu=native` in the rustflags is an error. `Lowered::ran` and `Lowered::macros` wait
until every snapshot is dropped, including one held in a `HirFrontend`'s `Databases` closure,
so drop those before the loop applies a run. The rest is in
`/tmp/blaze-lanes/v2-cargo-daemon-lower.md`.

**From cargo-daemon-lower (to cargo, `crates/load-cargo/src/lib.rs`), docs/v2/plan/design/unsafe.md:**
`crates/load-cargo` cannot say `#![forbid(unsafe_code)]` crate-wide while v1's `lib.rs` has its
two `unsafe { SyntaxContext::from_u32(ctx) }` (lines ~1024, ~1071). `v2` is `#![deny(unsafe_code)]`,
with `forbid` on `v2/crates.rs`, `v2/files.rs` and `v2/tests.rs`, and a scoped
`#[allow(unsafe_code)]` on `mod db` only for the `unsafe impl`s salsa's macros expand to. When v1's
load goes at integration, forbid it in `lib.rs` (still `allow` on `v2::db` for salsa).

> applied: lane ci is DONE. `Request::Stop` (request 11), `Response::Fetched { verb: FetchVerb, what: String }` and `FetchVerb` in `crates/blaze-api/src/lib.rs`; `Session::report` passes it on as any response. The asked `Session::finish_in(cx, job, ok, error, queries: Option<ViewId>, out)` is in `session.rs`: the session keeps the handed view for `Why` in place of its own (closing its own), and closes it at once when the job's view is not kept or the job is unknown; `finish` is `finish_in(.., None, ..)` (test `why_keeps_the_view_a_build_ran_in_when_the_daemon_hands_it_over`). The daemon calls `finish_in` and stops closing that view. > forwarded to core: `scratch(dir)` on each driver. > forwarded to coordinator: `crates/blaze/src/main.rs`'s counting `#[global_allocator]` is integration, not among the paths `requests` applies. The notes to cargo-load, cargo-cli and cargo-daemon-units are inside lane cargo.

## 14. From cargo-load (to lane cargo, with request 1): the offline note, `-Z bindeps` and artifact dependencies (#601)

`crates/blaze-units/src/load/` is written against these three; it does not compile until they
are in.

**`crates/blaze-resolve/src/workspace_resolve.rs`:** `Workspace` gets
`pub offline_flag: Option<&'a str>` (`--offline`, `--frozen`, or `net.offline` as `--offline`),
and `resolve_ws` passes it on: `ResolveSettings { offline_flag: ws.offline_flag,
..ResolveSettings::default() }` in place of `ResolveSettings::default()` (line ~737). **Why:**
cargo's "note: offline mode (via `--offline`) can sometimes cause surprising resolution
failures" is in `errors.rs` already but nothing sets it; `load/resolve.rs` fills the field.

**`crates/blaze-manifest/src/config/mod.rs`:**

```rust
impl Config {
    /// cargo's `CliUnstable` of the `-Z` flags (each as given after `-Z`, `name` or
    /// `name=value`) and of the `[unstable]` table, as cargo's `GlobalContext::configure` reads
    /// them on a nightly. A flag blaze does not read is cargo's "unknown `-Z` flag specified: {name}".
    pub fn cli_unstable(&mut self, flags: &[String]) -> anyhow::Result<()>;
    /// `-Z bindeps`.
    pub fn bindeps(&self) -> bool;
}
```

**`crates/blaze-manifest/src/manifest.rs` and `dependency.rs`:** with `config.bindeps()`, a
dependency's `artifact`, `lib` and `target` are read as cargo's `to_dependency` reads them
(`Artifact::parse`: the kinds `bin`, `bin:<name>`, `cdylib`, `staticlib`; `lib` defaulting to
false; `target` a triple or `"target"`; cargo's errors for an empty or unknown kind), into

```rust
pub struct Dependency {
    // ... every field as it is, and:
    pub artifact: Option<Artifact>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    /// As written: `bin`, `bin:<name>`, `cdylib`, `staticlib`.
    pub kinds: Vec<String>,
    pub lib: bool,
    /// `target = "<triple>"`, or `"target"` for the build's own.
    pub target: Option<String>,
}
```

and `SerializedDependency` writes it as cargo metadata does (`"artifact": {"kinds": [...],
"lib": .., "target": ..}`, left out when `None`). Without `-Z bindeps` the error stays cargo's
"`artifact = …` requires `-Z bindeps` ({name})". **Why:** #601 asks that a workspace with an
artifact dependency load and resolve, and that only a unit that depends on one be refused
(`load/units.rs` `artifact_refusal`, which reads `dep.artifact`). `load/config.rs` calls
`config.cli_unstable(&read.unstable)` after `start`, and `load.rs` accepts `-Zbindeps`.

**Done in cargo-load's own paths (no action):** `sha2 = "0.10"` (optional, in `query`) for
cargo-load-source's `tree.rs` (request 10); `#![forbid(unsafe_code)]` in
`crates/blaze-units/src/lib.rs` and `crates/blaze-tree/src/lib.rs` (cargo-std's request 10).
`StdKey.{host, rustflags, profile_root}` are as cargo-std's request 6 has them.

> forwarded to coordinator: `blaze-resolve` and `blaze-manifest` are "every other path" in `crates.md`, the coordinator's, and go with request 1's grant to lane cargo; not among the paths `requests` applies.

## From cargo-daemon-units (`crates/blaze-cargo/src/serve/unit.rs`, `serve/unit/`)

`serve/unit*` calls each item below by this signature already; each waits on the lane named.
The full list, with why, is in `/tmp/blaze-lanes/v2-cargo-daemon-units.md`.

**To cargo-daemon-lower (inside lane cargo, `crates/load-cargo/src/v2.rs`):**
`Lowered::identity(&self, unit: usize) -> Option<ContentId>`: the identity of everything the
unit's crate is made from, deeply (its files' contents, cfgs, env, edition, its dependencies'
identities and the results of the build script runs it was given). The unit's record in the
store is keyed by it; `None` means "keep nothing".

**To cargo-load (inside lane cargo, `crates/blaze-units/src/query.rs` and the load):**
- `UnitPlan::unsandboxed: bool`: the package is named in the workspace's
  `unsandboxed-build-scripts` (or a member's `build-script = false`), as docs/cargo.md says.
- `UnitPlan::runner: Vec<String>`: `target.<triple>.runner` / `CARGO_TARGET_<TRIPLE>_RUNNER`
  of the unit's platform, program then arguments; empty for none (`Artifact::runner`).
- `UnitPlan::harness: bool`: the target's `harness` (`Artifact::harness`).
- `PackagePlan::default_run: Option<String>`: the manifest's `package.default-run`.
- `Units::link_dirs: Vec<String>`: the workspace's `build-script-link-dirs`.

**To cargo-std (inside lane cargo):** `blaze_units::stdlib::library(launcher: &Path) ->
Result<Library { at: String, files: Vec<LibraryFile { path, bytes, executable: bool }> }, String>`.

**To cargo-daemon (inside lane cargo, `serve.rs`, `serve/work.rs`, `crates/blaze-cargo/Cargo.toml`):**
- Dependencies of blaze-cargo: `blaze-diagnostics`, `blaze-items`, `hir`, `hir-def`, `hir-ty`
  (`workspace = true`).
- The test queue's root is `Paths::state` (`tests/<tags hex>` under it); say if it is not.
- A loop hook after a proc-macro unit's compile, to apply its expanders with
  `Lowered::macros(unit, result)` (it waits for every snapshot to drop, so not on a worker):
  e.g. `Front::built(&self, unit: usize, compiled: &Compiled) -> Result<(), ApiError>`.

**To lane codegen (`crates/hir-ty-codegen/v2*`, `crates/blaze-codegen`):**
- `hir_ty_codegen::v2::toolchain(target: &str, rustflags: &[String]) -> Result<Toolchain, String>`.
- `hir_ty_codegen::v2::ItemHashes::new(Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>)`
  implementing the driver's `Hashes` (with lane items).
- `RootKind { Executable, Test, Library, Shared, ProcMacro }` and
  `HirFrontend::roots(&self, krate: base_db::Crate, RootKind) -> Result<Roots { roots: Vec<HirInstance>,
  tests: Vec<RootTest { name, root: usize, ignore: bool, should_panic: Option<Option<String>>,
  bench: bool, outside: bool }> }, CodegenError>`; a test binary's roots include libtest's
  start with the fork server.
- `HirFrontend::linked(&self, krate) -> Result<Vec<NativeLib>, CodegenError>`: the crate's
  `#[link]`s, in order.
- `HirFrontend::link_inputs(&self, &Built) -> Result<LinkInputs { spans: Vec<(ContentId, Vec<u8>)>,
  debug_names: Vec<(InstanceKey, String)> }, CodegenError>`.
- `Built::reached(&self, root: &InstanceKey) -> Vec<CodeHash>`: the code a root reaches, for
  test selection.
- `blaze_codegen::{NativeLib { name, kind: NativeLibKind, verbatim: bool },
  NativeLibKind { Dylib, Static { whole, bundle }, Framework }}`, `PartialEq + Debug`.
- A proc-macro crate built for `wasm32-wasip1` with blaze-macro-wasm's `GUEST` (docs: proc macros
  run as wasm); until then a proc-macro unit is linked as a native shared library and not loaded.

**To lane link (`crates/blaze-link`):**
- `blaze_link::target::output(triple: &str, shared: Option<(String, Vec<String>)>, debug: Debug,
  incremental: bool) -> Result<Output, LinkError>`.
- `blaze_link::target::natives(&Output, &[NativeLib], &[PathBuf]) -> Result<Vec<OwnedNative { id,
  name, bytes, kind }>, LinkError>`: C runtime objects first, the libraries in order, the
  system's last.
- `Image::bytes(&mut self, Staged) -> Result<Vec<u8>, LinkError>`.

**To lane usercode (`crates/blaze-usercode`):** a way for `test::Queue` to run a binary outside
the sandbox, for `BLAZE_SANDBOX=off` and `unsandboxed-tests`; until then either is an error naming it.

**To lane no-rustc / cargo-cli:** the launcher started as `rustc` or `rustdoc` (the stand-ins a
build script is given as `RUSTC`/`RUSTDOC`, `<state>/tools/`) must answer as blaze v2, not v1's
`as_rustc`.

> forwarded to cargo (its own children), codegen, link and usercode: every item names a lane that is still running (`ItemHashes::new` is now codegen-hashes', codegen.md 6). Nothing asked of the coordinator's paths.

## From speed-runtime-plane: `crates/blaze-cargo/src/serve/plane.rs`, one output buffer (2026-10-29)

- What: `blaze_graph::Plane` gained `step_into(&mut self, PlaneInput, &mut Vec<PlaneOutput>) ->
  Result<(), Error>` (appends; nothing on an error) beside `step`. In `serve::plane::Plane`, keep a
  `said: Vec<PlaneOutput>` field and write `fn step` as
  `let mut said = std::mem::take(&mut self.said); let taken = self.graph.step_into(input, &mut said);
  if taken.is_ok() { self.outputs(said.drain(..)) } self.said = said; taken`, with `outputs`
  taking `impl Iterator<Item = PlaneOutput>` (its body is unchanged).
- Why: the daemon steps the plane once per task; this takes the one allocation per step the
  plane still made, with the same outputs.

> forwarded to cargo: `crates/blaze-cargo/src/serve/plane.rs` is lane cargo's.

## From safety (lane safety, 2026-10-29): `blaze test --ub`, and `-Zsanitizer` once codegen has it

1. **`blaze test --ub [--ub-seed N] [--ub-schedules K]`** (`crates/blaze-cargo/src/cli/args.rs`,
   `cli/test.rs`, and the daemon's test path): opt-in; without the flag nothing changes. With it,
   each selected test function (the same selection `blaze test` makes: filters, `--exact`,
   `#[ignore]`) runs in blaze's MIR evaluator under `hir_ty_ub::run_test(db, test_fn,
   &hir_ty_ub::Config { seed, schedules, ..Default::default() })` instead of a compiled
   binary, one plane task per test, with the daemon's `HirDatabase`. `hir_ty_ub::Outcome`
   maps to a test result: `Passed` passes; `Failed { message }` fails as a panicking test does
   (honoring `#[should_panic]`); `Ub { report, seed, .. }` fails with `report` as its output
   and the seed to replay; `Unsupported { what }` fails, naming what the evaluator can't run
   (no-fallback: never a pass, never a compiled run in its place). Nothing is compiled or
   linked for these tests, and no rustc runs. If the CLI reaches the daemon through
   `blaze-api`'s test request, that request gains the three fields (an interface change: ask the
   coordinator in your request file). `hir-ty-ub` as a dependency of `blaze-cargo`: the workspace
   entry is requested in `v2/requests/safety.md` item 2.
   Why: docs/v2/plan/design/unsafe.md "Our own Miri": every unsafe module's tests run under it.
   A superset feature: opt-in, and Rust's behavior is unchanged (superset-of-rust).
2. **`-Zsanitizer` and `-Zsanitizer-coverage`** (`crates/blaze-cargo/src/unstable.rs:237-243`):
   when codegen's `v2/requests/codegen.md` "From safety" S2 lands (the LLVM back end
   instruments and links the runtimes), change `sanitizer`, `sanitizer-recover`,
   `sanitizer-memory-track-origins` and the coverage flags from `Refused(INSTRUMENTS)` to
   passed to codegen, and keep refusing what codegen still doesn't do (CFI, KCFI, dataflow) with
   that reason. Until then they stay refused.

> forwarded to cargo: `blaze test --ub` and `unstable.rs` are lane cargo's. `hir-ty-ub` is now a workspace dependency (safety.md 2).

## 15. From cargo-cli (to the coordinator and ci, `crates/blaze-api/src/lib.rs`; and to cargo-daemon): `blaze test --ub` (answers lane safety's item 1)

The CLI side of `blaze test --ub [--ub-seed N] [--ub-schedules K]` is written
(`cli/args.rs`, `cli.rs`), against this interface change, which it does not compile without:

```rust
pub enum Request {
    /// Runs the tests the view's change reached (all, with `all`), filtered by name, with
    /// libtest's selection `args`; in blaze's MIR evaluator, checked for undefined behavior,
    /// with `ub`.
    Test { all: bool, view: ViewRef, filter: Vec<String>, targets: Targets, args: Vec<String>, ub: Option<Ub> },
    // ... the rest as they are
}

/// `blaze test --ub`: each selected test run under `hir_ty_ub::run_test`, nothing compiled.
/// `None` is `hir_ty_ub::Config::default()`'s.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ub {
    pub seed: Option<u64>,
    pub schedules: Option<u32>,
}
```

`args` is what follows `--` (`--exact`, `--ignored`, `--include-ignored`, `--skip NAME`): the
daemon needs it to select the tests as libtest would, for `--reached` too, and the CLI has it
nowhere else. Non-UTF-8 arguments are refused by the CLI.

What the CLI does: `--ub` (or either value, which implies it) is `test`'s alone and never on
cargo's command line. It sends `Request::Test { all: !reached, ub: Some(..), .. }`, and
`--ub` with `--no-run` is refused. The daemon's `Response::TestResult`s are printed in libtest's
pretty format, and any failure exits 101. To cargo-daemon: an `Outcome::Ub { report, seed, .. }`
is a `TestResult` with outcome `Failed` and `output` = the report, then `seed: N` (the CLI
prints the output as libtest prints a failure's stdout). `Unsupported { what }` is `Failed` with
`what` as its output, never a pass and never a compiled run (no-fallback). `binary` is the test
target's would-be executable path, or its name, which the CLI shows after `Running`.

> applied: lane ci is DONE. `crates/blaze-api/src/lib.rs`: `Request::Test` gains `args: Vec<String>` and `ub: Option<Ub>`, and `Ub { seed: Option<u64>, schedules: Option<u32> }` as written; `SCHEMA` is 7. `Work::Test` (`build.rs`) carries both to the daemon, the session passes them through (test `test_run_and_verify_start_their_work` now sends both), BestOf's tests send none, and blaze-ci's `Check::request` sends `args: Vec::new(), ub: None`. > forwarded to cargo: `serve/job.rs:1451` and `serve/tests.rs:656` build a `Work::Test`/`Request::Test` and need the two fields; the daemon runs `ub` and `args`.

## From core (2026-10-29): `crates/blaze-cargo/src/serve/plane.rs:494`, `PlaneOutput::Run` gains `critical`

What: `blaze_graph::PlaneOutput::Run` gains `critical: bool` (the node is on its request's
critical path). `serve/plane.rs:494` matches `PlaneOutput::Run { id, pool, near }` with no
`..`, so it stops compiling once the field lands. Match `Run { id, pool, near, critical }` and,
where the task goes to the pool, submit it with `Pool::submit_critical` when `critical`
(as `blaze_graph::exec` does), so a critical task runs on a fast group (P cores on Apple).
Why: docs/v2/plan/design/hardware.md, threads row: the critical path on the fast cores.
