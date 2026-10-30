---
status: plan
summary: blaze v2's crates: which crate holds which interface, which lane owns which paths (never overlapping), and what v2 does to each existing crate.
date: 2026-10-29
---
# blaze v2: crates and owners

Step 0 of [building.md](../process/building.md) section 12, as of 2026-10-29. The interfaces every lane programs
against are the public items of the crates below as step 0 left them: types, traits and doc
comments; step 0 left each body a lane writes as a panic naming the lane.

## Rules

- **A lane owns paths, listed below; no path has two owners.** It writes bodies, private
  items, tests, new public items and its crates' own `Cargo.toml` (with `workspace = true`
  dependencies) freely.
- **The interface items** (every public item step 0 wrote, listed per crate below) change
  only through a request: append what, where and why to `v2/requests/<lane>.md`; the
  coordinator changes it for every lane at once. The root `Cargo.toml` and `Cargo.lock` are
  the coordinator's too: a new workspace dependency is a request.
- **IDs**: hashes, keys and outputs use [`ContentId`](../../../../crates/blaze-ids/src/lib.rs) only,
  each made with a domain string naming what it is and its schema version
  (`"blaze v2 <what> <n>"`). A dense `Idx<T>` never goes into a hash, a key or an output.
- **Step 0's unfinished bodies are the owning lane's to finish**, every one: a lane is not
  done while the check of `common.md` (no `unimplemented` or `todo` macro) finds anything in
  its paths. What it can't finish for a real reason goes in its request file and its report;
  never a default, never a path back to v1.
- A lane's design notes go in `docs/v2/plan/lanes/<lane>/` (`notes.md` and any others) (front matter `status: plan`), its
  own; its plan, `docs/v2/plan/lanes/<lane>.md`, changes by request.

## The v2 crates

Each crate depends only on crates above it in this table (and on existing crates such as
`blaze-protocol`).

| crate | holds | owner after step 0 |
|---|---|---|
| `blaze-core` (changed) | effects, completions, `Driver`, the drivers (simulation, threads, io_uring); step 0 added `Map`, `Spawn` (with `Command`, `Sandbox`, `Traced`, `Exit`), `Kill`, `Listen`, `Unlisten`, `Connect`, `Recv`, `Send`, `Hangup` | core |
| `blaze-graph` (changed) | `plane` (requests, `RequestId`, `Key`, `Budget`, `PlaneConfig`, `PlaneInput`, `PlaneOutput`: memo, single flight, fair share, critical path, admission), `admission` (`TaskKind`, `Estimate`, `Estimates`, `Arena`), `machine` (`Spec` gained `key` and `memory`), `exec` | core |
| `blaze-ids` | `ContentId` and its `Hasher` (BLAKE3, domain-separated), `Fingerprint` (128 bits), dense `Idx<T>`, the `Interner` trait mapping one to the other | format |
| `blaze-format` | the zero-copy format: `Record` (unsafe trait), `Rel`, `Slice`, `Str`, `Header`, `SchemaHash`, `Writer`, `open`, `Persist` (what a query returns, read in place as `Persist::Ref`), `SpanRef`, `FormatError` | format |
| `blaze-vfs` | the Merkle VFS (port blaze-next's `crates/blaze-vfs` into it): `View`, `ViewId`, `File`, `Mode` (regular, executable, symlink, hashed into the tree as git's 100644, 100755 and 120000), `Op`, `OpError`, `Change`, `ChangeKind`, `Commit`, `CommitError`, `Ticket`, `Recovered`, `Vfs` (heads, the empty root, views, reads, symlinks followed, diff, journal through effects, collection), `Watcher` and `Watched` (the disk as transactions), `Reader` (`Vfs::reader`: any root's files from any thread; blaze-query implements its `Tree` for it). Its view arena is the one every crate uses: a `ViewId` is a `Vfs::open` of a root, and no other crate keeps its own | vfs |
| `blaze-query` | the query database's surface: `QueryKind` and `kinds` (number ranges per lane), `QueryKey` (and its `id`, the plane's task key), `Edge`, `Memo`, `Stored`, `Input`, the `Query` trait, `Cx` (records edges), `Cancelled`, `Why` and `Changed`, `DbError`, `Database` | db |
| `blaze-items` | item identity and content: `ItemKey`, `ItemKind`, `InterfaceHash`, `LayoutHash`, `CodeHash`, `DeepHash`, `ItemHashes`, `SimplifiedType`, `NameHash`, `Fact` (section 6.2's table and the cross-item rules) with `Fact::key`, the fact `kinds`, `ImplIndexKey`, `ScopeIndexKey`. A front-end crate: it gains `hir-ty` dependencies for the hashing (port blaze-next's `crates/blaze-items`) | items |
| `blaze-cas` | the store: `Key`, `Kind`, `Entry`, `Ticket`, `Corrupt`, `Mismatch`, `Config`, `Store` (open, get verified, has, put, flush, verify, place, completions), the `Remote` tier | store |
| `blaze-codegen` | the unit of codegen: `Instance`, `InstanceKey`, `Settings`, `Backend`, `Arch`, `Unit`, `CompiledFunction` with `Reloc`, `RelocKind`, `Target`, `Data`, `CodegenError`, the `Compile` trait | codegen |
| `blaze-link` | the in-place linker: `Slot`, `Change`, `Staged`, `LinkError`, `Image` (stage, commit, roll back) | link |
| `blaze-usercode` | user code: `RunKind`, `RunKey` (code and given, then traced inputs), `Reached` (a test's code hashes) | usercode |
| `blaze-api` | the agent API: `SCHEMA` (5), `Envelope`, `ViewRef`, `Targets` (with `Cargo`), `Request` (commit, check, build, test, run, open, close, reach, best-of, verify, snapshot, why, diff, status, cancel, codegen inputs, trace, tables), `Response`, `Diagnostic` (with `FactRead` facts and rustc's `rendered` text), `Level`, `Span`, `Artifact`, `TestResult`, `TestOutcome`, `Reach`, `Ranked`, `Mismatch`, `Worked`, `Status`, `ApiError`, in `blaze-protocol`'s deterministic CBOR and frames; the daemon's side `build::{Builder, Work, JobId, Opened}`; `session`, `server`, `http`, `client` | ci |
| `blaze-ci` | the VM as CI: `Event`, `CheckRun`, `CheckStatus`, `Train`, `Checkout` | ci |
| `blaze-v2-proof` | the oracle, the corpus differential, budgets (created by proof) | proof |

How they meet: a request comes in through `blaze-api`, opens a `blaze-vfs` view, and asks
`blaze-query` queries; each query execution is a plane task (`QueryKey::task_key`), admitted
against `admission::Estimates`; queries read files through `Cx::file` and facts through
`blaze-items`' `Fact::key`; results live in `blaze-format` buffers and, once cold, in
`blaze-cas` under their key; codegen queries return `CompiledFunction`s keyed by
`InstanceKey`, which `blaze-link` places and commits; build scripts and proc macros are
`Effect::Spawn`s in a tracing `Sandbox`, keyed by `blaze-usercode`'s `RunKey`.

## Paths each lane owns

| lane | paths |
|---|---|
| coordinator (`v2`) | root `Cargo.toml`, `Cargo.lock`, `docs/v2/` but the lanes' notes folders (`docs/v2/plan/lanes/<lane>/`), `v2/requests/` (applying them), `crates/hir-ide/`, `crates/ide-db/`, `crates/test-fixture/` until they are removed |
| cargo | `crates/blaze-cargo/` but store's and usercode's paths, `crates/load-cargo/`, `crates/project-model/`, `crates/blaze-units/`, `crates/blaze-tree/`, `library/` |
| diagnostics | `crates/blaze-diagnostics/` (new), `crates/ide-diagnostics/` |
| core | `crates/blaze-core/`, `crates/blaze-graph/`, `docs/v2/plan/lanes/core/plane.md` |
| vfs | `crates/blaze-vfs/`, `crates/vfs/`, `crates/vfs-notify/` |
| format | `crates/blaze-ids/`, `crates/blaze-format/`, `crates/syntax-tree/`, `crates/syntax/`, `crates/parser/`, `crates/span/`, `crates/intern/`, `crates/tt/`, `crates/mbe/`, `crates/syntax-bridge/`, `crates/const-ast/`; in `crates/hir-def/src/`: `expr_store.rs`, `expr_store/`, `hir.rs`, `hir/`, `item_tree.rs`, `item_tree/` |
| db | `crates/blaze-query/`, `vendor/salsa/`, `vendor/rustc_type_ir/`, `vendor/rustc_next_trait_solver/`, `vendor/salsa-macros/`, `vendor/salsa-macro-rules/`, `crates/base-db/`, `crates/hir-expand/`, `crates/hir/`, `crates/hir-def/` but format's files, `crates/hir-ty/` but items' files, `crates/hir-ty-borrowck/`, `crates/hir-ty-wfcheck/` |
| items | `crates/blaze-items/`; in `crates/hir-ty/src/`: `method_resolution.rs`, `method_resolution/`, `traits.rs`, `specialization.rs`, `opaques.rs`, `layout.rs`, `layout/`, `drop.rs` |
| store | `crates/blaze-cas/`, `crates/blaze-store/`, `crates/blaze-cargo/src/remote/`, `crates/blaze-cargo/src/remote.rs` |
| codegen | `crates/blaze-codegen/`, `crates/hir-ty-codegen/`, `crates/hir-ty-mir/`, `crates/blaze-engine/`, `vendor/llvm-sys/` |
| link | `crates/blaze-link/`, `crates/blaze-splice/`, `crates/blaze-live/`, `crates/blaze-in-place/`, `vendor/cranelift-object/` |
| usercode | `crates/blaze-usercode/`, `crates/blaze-build-script/`, `crates/blaze-expand/`, `crates/blaze-macro-wasm/`, `crates/blaze-wasi/`, `crates/proc-macro-api/`, `crates/proc-macro-srv/`, `crates/blaze-cargo/src/sandbox.rs`, `crates/blaze-cargo/src/sandbox/` |
| ci | `crates/blaze-api/`, `crates/blaze-ci/`, `crates/blaze-protocol/`, `crates/blaze-ui/`, `crates/blaze-web/` |
| proof | `crates/blaze-v2-proof/`, `harness/v2/` |

Every other path (the cargo crates `blaze-manifest`, `blaze-metadata`, `blaze-new`,
`blaze-registry`, `blaze-resolve`, `blaze-source`, `blaze-tree`, `blaze-units`; `cfg`,
`edition`, `paths`, `stdx`, `profile`, `toolchain`, `macros`, `rustc_abi`, `test-utils`,
`blaze-doc`, `blaze-fmt`, `blaze-lints`, `blaze-fuzz`, `blaze-bench`, `blaze-cache`,
`blaze-cache-macros`, `blaze-wasm`,
`library/`, `harness/` but `harness/v2/`, `.github/`, `docs/` but the lanes' notes) is the
coordinator's: a change there is a request.

## What v2 does to each existing crate

| crate | v2 | by |
|---|---|---|
| `blaze-core` | changed: the new effects' drivers (Linux, macOS, simulation) | core |
| `blaze-graph` | changed: the plane becomes the one scheduler, `Machine` its one-request case; the executor's cache-aware groups | core |
| `blaze-store` | replaced by `blaze-cas`, deleted at integration | store |
| `blaze-cargo` | changed: its daemon and client move onto `blaze-core`, `blaze-graph`'s plane and `blaze-api` at integration; its sandbox gains tracing (usercode); its remote store becomes the read-through tier (store) | coordinator, usercode, store |
| `blaze-engine` | replaced: the crate-at-a-time codegen tail becomes per-function units | codegen |
| `hir-ty-codegen` | changed: compiles one `Unit` to one `CompiledFunction` | codegen |
| `hir-ty-mir` | changed: instances keyed by `Instance`, code dedupe, inlining edges | codegen |
| `blaze-splice`, `blaze-in-place` | replaced by `blaze-link` | link |
| `blaze-live` | changed: patches through `blaze-link`'s slots and commit | link |
| `blaze-expand` | replaced: expansions keyed by `RunKey` with traced inputs | usercode |
| `blaze-build-script`, `blaze-macro-wasm`, `blaze-wasi`, `proc-macro-api`, `proc-macro-srv` | changed: run in tracing sandboxes, from a warm pool | usercode |
| `vfs`, `vfs-notify` | replaced by `blaze-vfs` and `blaze-core`'s watch | vfs |
| `syntax-tree`, `syntax`, `parser` | changed: one flat tree per file replaces rowan (#42) | format |
| `span`, `intern` | changed: spans in side tables; sharded interners | format |
| `tt`, `mbe`, `syntax-bridge`, `const-ast` | changed: onto the flat tree | format |
| `hir-def` | changed: SoA HIR and outlines (format); signature and body split, parallel expansion (db) | format, db |
| `hir-expand`, `base-db`, `hir` | changed: queries keyed by input hashes with fingerprints, inputs per view | db |
| `vendor/salsa*` | changed: remade into blaze's query database behind `blaze-query` | db |
| `hir-ty` | changed: facts recorded where they are read (items); queries split and fingerprinted (db) | items, db |
| `hir-ty-borrowck`, `hir-ty-wfcheck` | changed only as `hir-ty` moves | db |
| `blaze-protocol` | left: `blaze-api` uses its CBOR and frames; its `events` stay for the build view | ci |
| `blaze-ui`, `blaze-web` | changed: clients of `blaze-api` | ci |
| `hir-ide`, `ide-db`, `test-fixture` | removed at integration | coordinator |
| `ide-diagnostics` | replaced by `blaze-diagnostics` ([building.md](../process/building.md) section 18), then removed | diagnostics |
| `load-cargo`, `project-model` | replaced by the workspace load as queries ([building.md](../process/building.md) section 18) | cargo |
| `blaze-fmt`, `blaze-lints` | left; moved onto the flat tree at integration | coordinator |
| every other crate | left | coordinator |

## Left for the lanes to decide

- The flat syntax tree's and SoA HIR's types are format's own, in the crates it owns; no
  other lane programs against them before integration.
- `Query::Key`'s shape per query, the db's memo table layout, the plane's learning rule, the
  store's pack and index layout, the linker's chunking: the owning lane's.
- `Cargo.lock` gains `blake3` (a new workspace dependency, for `blaze-ids`) and the new path
  crates at the coordinator's first build.
