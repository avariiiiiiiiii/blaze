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
