---
status: plan
summary: blaze v2 lane cargo: the workspace load and unit graph as queries over a view, the standard library as one more crate graph, the daemon on the plane and the agent API, and the CLI as its client with cargo's flags, output and exit codes.
date: 2026-10-29
---
# blaze v2: lane cargo's design

Lane cargo's notes, as of 2026-10-29: [building.md](../../process/building.md) section 18, #1450, #805, #804, #86, #91, #298, #601.
Requests are in `v2/requests/cargo.md`; the shared types in
`crates/blaze-units/src/query.rs` (feature `query`).

## Parts and owners

| part | paths | child |
|---|---|---|
| the interface: view configuration, mounts, `Plan`, `StdUnits`, `Cbor<T>` | `crates/blaze-units/src/query.rs` | cargo (the lane) |
| the workspace load and the unit graph as queries | `crates/blaze-units/` but `query.rs` and `src/stdlib*`; `crates/blaze-tree/` | cargo-load |
| the standard library as a crate graph; target cfg without rustc | `crates/blaze-units/src/stdlib.rs`, `crates/blaze-units/src/stdlib/`, `crates/project-model/`, `library/` | cargo-std |
| the daemon on the plane, `blaze-core` and `blaze-api`; lowering units to the front end | `crates/blaze-cargo/src/serve.rs`, `crates/blaze-cargo/src/serve/`, `crates/load-cargo/` | cargo-daemon |
| the CLI as a client of the agent API | `crates/blaze-cargo/src/cli.rs`, `crates/blaze-cargo/src/cli/` | cargo-cli |

v1's code in these crates (`blaze-cargo/src/cargo/`, `socket*`, `plan.rs`, `load-cargo`'s
rust-analyzer load, `project-model`'s `ProjectWorkspace`) stays until integration deletes it; v2's
code calls none of it but pure helpers (flag parsing, cargo's message text), and nothing of v2
falls back to it.

## 1. A build is a view with a configuration

A request names a view (`blaze_api::ViewRef`); the session opens it in the VFS and the database
and hands the daemon's builder an `Opened`. The builder opens the same root again in the database
with a `BuildConfig` (`Database::open_with(view, &config.encode())`), the only thing
`Cx::config` reads:

- `invocation`: the cargo command line (`blaze_api::Cargo`, schema 5: `v2/requests/cargo.md`
  request 3), or one the daemon makes of an agent's `Targets` (`-p`, `--profile`, `--target`,
  `-F`, `--all-features`, `--no-default-features`, cwd at the root, an empty environment but
  `PATH` and `HOME` of the daemon).
- `mounts`: where every directory cargo names is: the view at the client's checkout path, each
  fetched source where cargo keeps it, the standard library at `/rustc/<commit>/library`.
- `host` and `library`.

Only the load's first query reads the configuration; it keeps of the environment only the
variables cargo reads by name (`CARGO_*`, `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `RUSTDOCFLAGS`,
`CARGO_TARGET_*`, `CARGO_PROFILE_*`, `CARGO_BUILD_*`, `CARGO_HOME`, `HOME`), so no other value is
in a query's value, the image or the store. Build scripts read the rest of it as a traced input
(usercode), so `PATH` keys what read it, never a worker (#805). The whole environment lives in
memory for the request only and is never logged.

## 2. Every file through the database

The workspace load reads by absolute path, as cargo names it. A path's longest `Mount` gives its
origin: the view (`Cx::file`, `Cx::dir`) or another root (`Cx::file_at`, `Cx::dir_at`: request
2). Each read is an edge, so a result is keyed by exactly the files it read: an edit to a source
file reruns nothing of the load, a `Cargo.toml` edit reruns that manifest's queries and what
their values reach (early cutoff below that). A path under no mount fails the load naming it;
nothing reads the disk (no-fallback). blaze-manifest and blaze-metadata read through a `Files`
trait (request 1); the load's `Files` is over `Cx` and the mounts.

## 3. Sources

A registry package is its `.crate`, checked by the lock file's sha256; a git package its tree at
the locked commit; a sparse index file as served. Each is fetched once by the daemon, never by a
query: `Plan` answers `Needs(fetches)` with what it would read that no mount covers; the daemon
fetches them (IO tasks of the plane), checks each (sha256 for a `.crate`, path entries that would
escape refused), puts each into the VFS as a root of its own, keeps `at -> root` durable in its
journal, opens the view again with the new mounts and asks again. A source's root is a function
of its bytes, so every view and project with the same version shares one root and every result
keyed by it: the shared base layer. `--offline` and `--frozen` make a needed fetch an error naming
the package, as cargo's.

## 4. The queries of the load

Kinds in `0x6000..0x6800` (request 2): `PLAN` and `STD_UNITS` fixed in `query.rs`; cargo-load's
own in `0x6010..0x6400`, cargo-std's in `0x6400..0x6800`. Each value is a `Cbor<T>` with a
versioned schema. cargo-load splits the load so each query reads the narrowest thing: the
invocation (the configuration, narrowed), the config chain (`.cargo/config.toml` files from cwd up,
`$CARGO_HOME/config.toml`, `--config`, `CARGO_*`), one manifest read and normalized, the workspace
root and its members, the lock file, the resolve with features (blaze-resolve, guided by the lock),
the unit graph (blaze-units' port of cargo), and per unit what cargo passes and writes
(`UnitPlan`: `-C metadata` and `extra-filename` as cargo computes them, so file names are
cargo's; cfgs, check-cfg, rustflags, lints, `CARGO_*` variables, outputs in cargo's layout). The
standard library's units come from `StdUnits` and are merged as `-Zbuild-std` merges them
(`is_std`, every unit depending on the std roots its kind needs).

cargo's refusals stay cargo's, word for word; what blaze has not ported is an error naming it
(#601: artifact dependencies are read and resolved, and only a build that includes a unit with one
is refused, naming it).

## 5. The standard library

`library/` (rust-lang/rust at `library/UPSTREAM`'s commit) is put into the VFS once as a root and
mounted at `/rustc/<commit>/library`, the paths rustc's own std has in panics and `file!()`. Its
workspace is loaded by the same queries, asked for the crates a build needs (`std`, or `core`
and `alloc` for `#![no_std]`; `proc_macro` for proc macros; `test` for test harnesses) with
`-Zbuild-std`'s features and the build's panic strategy, for each platform the build has (the host
for build scripts and proc macros, `--target`). std's build script runs sandboxed like any other.
The target's cfg, data layout and std's target facts come from blaze's own target table, never
`rustc --print` (no-rustc). Its results are keyed by the library's root, the target and the
profile's std settings, never the view: the shared base layer (#86).

Speed. A cache hit is the database's memo, so it runs none of this code and allocates nothing.
The measure for a miss is rustc's own `--print cfg`: about 20 ms, because it starts a process
and loads LLVM's target machine to list features. The data is compiled once per process into
per-target columns. Feature names are interned to bits of a 32-byte set; every recorded target
has at most 92 such names, and the set's limit of 256 is checked. Each feature's implications
and turn-offs are precomputed sets, as are the target spec's facts and every
`target_feature="…"` line. `target_cfg` is then a binary search for the triple, four word
operations per `-C target-feature`, and one sorted copy out, about a microsecond. Once set,
reads take no lock. The checks `StdUnits` makes of a loaded graph are O(units) with one
`FxHashMap` of package names. Not reached yet:
- The JSON (423 KB) is still parsed at the first call, about a millisecond per process. Reaching
  zero needs a build step that writes the table as `static` Rust data
  (`harness/answers.py --write` could emit it beside `rustc.json`).
- `StdUnits` decodes `Foreign`'s CBOR and writes it out again. Passing the bytes through needs a
  `Persist` for already-encoded CBOR in `query.rs`.

## 6. The daemon

`blaze serve` (`crates/blaze-cargo/src/serve*`): one process, one `blaze_core` driver, one loop:

- the agent API's `Server` (Unix socket, HTTP) and `Session`; the daemon is the session's
  `Builder`;
- the `Vfs` (journal, the workspace heads the CLI's checkouts are, their watchers);
- the `Database` (from its image at start, so nothing is loaded again: `Database::from_image_with`;
  `write_image` and `settle` after a cold build; `collect` between requests);
- the store (`blaze_cas::Store`);
- the plane: every job's queries, fetches, codegen units, links, build scripts and tests are its
  tasks, admitted against the memory budget, run on its worker pool.

A job asks `Plan`, fetches what it needs, then builds the units: the front end's inputs from the
units (load-cargo lowers `Units` to base-db's crate graph), build scripts and proc macros through
blaze-usercode, codegen through blaze-codegen's driver, linking through blaze-link, outputs placed
where `UnitPlan::outputs` says, and reports `Unit`, `Diagnostic`, `BuildScript`, `Built`. A
build's view is the checkout as saved: a CLI request names the head of its checkout, and the daemon
holds it until the head's watcher has seen every change made before the request (a cookie file
written by the daemon in the checkout's `target/` and waited for, as watchman does).

## 7. The CLI

`blaze build|run|test|check|bench` and `cargo blaze <same>`: parse as cargo does (v1's
`cargo/cli.rs` rules), find or start the daemon (`blaze serve`, socket under blaze's home), commit
nothing (the watcher has the edits), send `Request::Build` / `Check` with `Targets::cargo`, and
print what cargo prints, on the stream cargo prints it, with cargo's exit codes:

- `Compiling foo v0.1.0 (/w/foo)` per `Response::Unit`, rustc's text per diagnostic, `warning:
  `foo` (lib) generated 2 warnings`, `error: could not compile `foo` (bin "foo") due to 1 previous
  error`, `Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.12s`; with
  `--message-format json*`, cargo's JSON messages on stdout (`compiler-message`,
  `compiler-artifact`, `build-script-executed`, `build-finished`).
- `run`: builds, prints `Running `target/debug/foo``, and runs the program itself with the
  terminal, its arguments and the command's environment, exiting with its code.
- `test`/`bench`: builds the test targets, then runs each test executable (and the doc tests)
  as cargo does (`Running unittests src/lib.rs (target/debug/deps/foo-0123)`, `Doc-tests foo`),
  stopping at the first failing one without `--no-fail-fast`; exit 101 on a failure.
- exit codes: 0, 101 for a failed build or test (cargo's), 1 for cargo's usage errors.

The CLI reads no manifest itself: the daemon's load is the one load.
