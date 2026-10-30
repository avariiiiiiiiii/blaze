# Requests of lane cargo

Lane cargo's design is `docs/v2/cargo-notes.md`. Its children (cargo-load, cargo-std,
cargo-daemon, cargo-cli) append below, each request saying which child it comes from.

## 1. Ownership: blaze-manifest, blaze-metadata, blaze-resolve, blaze-registry, blaze-source to lane cargo

What: in `docs/v2/crates.md`, add `crates/blaze-manifest/`, `crates/blaze-metadata/`,
`crates/blaze-resolve/`, `crates/blaze-registry/` and `crates/blaze-source/` to lane cargo's
paths, and take `blaze-tree` and `blaze-units` out of the "Every other path" paragraph (the table
already gives them to cargo; the paragraph says they are the coordinator's).

Why: README section 18 gives cargo "everything cargo-shaped that no lane had", and the workspace
load as queries over a view can't be written without changing these crates. Today they read the
machine's disk directly: `blaze_manifest::manifest::read_manifest` (`std::fs::read`), target
autodiscovery (`targets.rs`: `exists`, `is_file`, `read_dir`), workspace member globs
(`manifest.rs`: `glob::glob`, `is_dir`), config discovery (`config/mod.rs`:
`read_to_string`, `exists`), blaze-metadata's reads of registry packages, and blaze-registry's
fetches. A query must read every file through `blaze_query::Cx` (so each read is an edge and
the result is keyed by what it read, docs/v2/README.md section 2), and a view is a Merkle root,
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
(`docs/v2/cargo-notes.md`, "Sources"). No behavior changes for a `Disk` context, which every
v1 caller keeps; `harness/resolve.py` still holds them to cargo. Lane cargo writes all of it once
granted; until then its child cargo-load writes everything else.

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
shared base layer (docs/v2/ci.md). Reading them through `Cx::file` would need every view to
contain every dependency's files. `Tree::file(root, path)` already reads any root; this only
exposes it to a query, with an edge, so `Why` can still name the file.

## 3. `crates/blaze-api/src/lib.rs`: what cargo's command line and cargo's output need (schema 5)

Why: the CLI (`blaze build|run|test|check`, `cargo blaze`) is a client of the agent API with
cargo's flags, output and exit codes unchanged (README section 18). `Targets` holds six of
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

## 4. Coordinator: the `blaze` program's entry (at integration)

`crates/blaze-engine/src/main.rs` (codegen's) runs `blaze_engine::main`, which runs v1's
`blaze_cargo::cargo::command`. At integration the `blaze` program (and `cargo-blaze`, which cargo
runs for `cargo blaze`) runs `blaze_cargo::cli::main(std::env::args_os())` (lane cargo's v2
client), and `blaze serve` runs `blaze_cargo::serve::main` (lane cargo's v2 daemon).
