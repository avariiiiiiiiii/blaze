# Requests of lane safety

Lane safety's design is `docs/v2/plan/lanes/safety/notes.md`; its plan is
`docs/v2/plan/design/unsafe.md`. Its children (safety-mem, safety-tb, safety-race,
safety-check, safety-fuzz, safety-model, safety-audit) append below, each under
`## From safety-<child>`.

## 1. Coordinator: workspace lints for unsafe code

What: in the root `Cargo.toml`, `[workspace.lints.rust]` `unsafe_op_in_unsafe_fn = "deny"`
(it is `"warn"` today, line ~257), and in `[workspace.lints.clippy]`
`undocumented_unsafe_blocks = "deny"` and `multiple_unsafe_ops_per_block = "deny"`; the same
in `RUSTFLAGS` of `.github/workflows/ci.yml` (the comment at the top of
`[workspace.lints.rust]` asks for it).
Why: docs/v2/plan/design/unsafe.md "Where unsafe is allowed". Child `safety-audit` is bringing
every crate up to them (its lists go to the owners); land the lints when its report says
DONE, or the coordinator's first build will stop at the crates it could not edit.

## 2. Coordinator: workspace members and dependencies

- `members = ["crates/*", "fuzz"]`: `fuzz/` (new, safety-fuzz) is outside `crates/*`.
- `[workspace.dependencies]`: `hir-ty-ub = { path = "crates/hir-ty-ub", version = "0.0.0" }`,
  for `blaze-cargo` (`blaze test --ub`, request 5).
- `crates/hir-ty-ub` itself needs nothing new: its dependencies are all workspace ones
  (`rustc-hash`, `smallvec`, `triomphe`, `rustc_abi`, `rustc_type_ir`, `base-db`, `hir-def`,
  `hir-ty`, `hir-ty-mir`, `intern`).

> applied: `hir-ty-ub = { path = "crates/hir-ty-ub", version = "0.0.0" }` in the root `[workspace.dependencies]` (its own dependencies are all workspace ones). `members` gains `"fuzz"` when `fuzz/` exists: a member path that doesn't exist stops the workspace from loading.

## 3. Coordinator: `docs/v2/plan/reference/crates.md`

Add lane `safety` to "Paths each lane owns": `crates/hir-ty-ub/`, `fuzz/`,
`crates/blaze-graph/src/check.rs` and `crates/blaze-graph/src/check/` (taken out of core's
`crates/blaze-graph/`), `docs/v2/plan/lanes/safety/`; and `hir-ty-ub` to the v2 crates table
("the checker of undefined behavior over hir-ty-mir's evaluator: `Checker`, `Config`,
`Outcome`, `run_test`", owner safety). A front-end crate (AGENTS.md semantics-in-front-end).

> forwarded to coordinator: `docs/v2/` is the coordinator's, not among the paths `requests` applies.

## 4. Coordinator: `docs/support.md`, once codegen's sanitizers land

The row at line ~345 refuses `sanitizer`, `sanitizer-recover`,
`sanitizer-memory-track-origins` (and the CFI flags) because "blaze's code generator does not
instrument". When codegen's request (v2/requests/codegen.md, "From safety", item S2) lands,
`-Zsanitizer=address`, `thread`, `memory` and `-Zsanitizer-recover`,
`-Zsanitizer-memory-track-origins` and `-Csanitizer-coverage-*` move to supported; what is
still refused (CFI, KCFI, dataflow, if codegen doesn't do them) stays, with that reason.

> forwarded to coordinator: `docs/support.md` is the coordinator's; it waits on codegen's S2.

## 5. Forwarded: what lane safety asked of others

- codegen (`v2/requests/codegen.md`, "From safety"): S1, the evaluator's `Machine` hooks in
  `crates/hir-ty-mir/src/mir/eval*` (codegen-mir owns it); S2, ASan, TSan, MSan and
  SanitizerCoverage in the LLVM back end, with their runtimes linked.
- db-ty (`v2/requests/db-ty.md`, "From safety"): `MirEvalError::MachineStop` in
  `crates/hir-ty/src/mir.rs`.
- cargo (`v2/requests/cargo.md`, "From safety"): `blaze test --ub`.
- core (through this file, from safety-model when it writes it): `pub mod check;` in
  `crates/blaze-graph/src/lib.rs`.

> applied: noted; each is in its lane's file, with its outcome there.

## From speed-link-edge-usercode (2026-10-29): parsers of untrusted bytes in its paths, for safety-fuzz

All are safe code (no `unsafe`, no zero-copy view), so the targets are for panics, hangs and
unbounded memory, and each has a safe model in its tests to compare against:

- `blaze_usercode::test::wire::Frames` (`new`, `push`/`push_owned`, `pop`) and `Frame::decode`:
  bytes a test binary (user code) writes on the fork-server socket. Split one input at fuzzed
  offsets and check `pop` gives the same frames as one whole `push` (the model is
  `frames_read_in_place_as_a_whole_stream_reads` in `test/tests.rs`); a frame longer than
  `LONGEST` must be an error, never an allocation of its length.
- `blaze_build_script::output::parse(input, cx)`: a build script's stdout. The model for which
  lines are directives is `a_line_is_a_directive_exactly_when_cargo_reads_one` in
  `output/tests.rs`.
- `blaze_build_script::elide::elide(root, files)` and `elide::verify(elided, ran)`: a build
  script's source, lexed by `elide/lex.rs` (string escapes, raw strings, integers).

`blaze-build-script` is now `#![forbid(unsafe_code)]` (lib and bin). Every `unsafe` block in
`blaze-usercode/src/test/fork_server.rs` and `blaze-cargo/src/sandbox/windows.rs` is one
operation with a `// SAFETY:` comment, and `interrupted` has a `# Safety` section, so neither
needs anything from safety-audit. `blaze-usercode` as a whole is the lead's (its `lib.rs`).

> forwarded to safety: for safety-fuzz, lane safety's.

## From safety-check

> forwarded to safety: C1 and C2 are the parent's `crates/hir-ty-ub/`. > forwarded to codegen: C3 (`crates/hir-ty-mir/src/mir/eval*`). > forwarded to db: C4 (`hir-def`'s lang items).

### C1. Parent: `crates/hir-ty-ub/Cargo.toml`

- `[dependencies]`: `either.workspace = true` (the call stack of `MirEvalError::InFunction`),
  `hir-expand.workspace = true`, `span.workspace = true`, `syntax.workspace = true` (a
  `Site`'s source place through the body's source map, as `hir-ide`'s `mir_pretty/errors.rs`
  `source_range`).
- `[dev-dependencies]`: `hir-ide.workspace = true` (`impl_hir_database!` for the tests'
  database), `salsa.workspace = true`, `test-fixture.workspace = true`,
  `test-utils.workspace = true`. No cycle: nothing under `hir-ide` depends on `hir-ty-ub`.

### C2. Parent: `crates/hir-ty-ub/src/ub.rs`

- `Ub::Value(it) => write!(f, "Undefined Behavior: {it}")`: `check::ValueError`'s Display is
  rustc's whole message, "constructing invalid value of type T: at .0.<enum-variant(Some)>.0,
  encountered 0x02, but expected a boolean" (`rustc_middle/src/mir/interpret/error.rs` line
  ~468), which puts the type and path before the message.
- A variant `Ub::ThreadsLeft` with Display "the main thread terminated without waiting for all
  remaining threads": Miri's leak check fails a run whose main thread returns while others
  still run (`src/tools/miri/src/eval.rs` line ~560), before it looks at memory.

### C3. Forward to the evaluator's owner (codegen-mir, `crates/hir-ty-mir/src/mir/eval*`)

1. Export `AtomicOrd` and `MResult` from `hir_ty_mir::mir` beside the others (the trait's
   signatures name them).
2. `Shadow::require_init` has no way to say *what* failed: it holds no machine. Add
   `fn require_init(&mut self, shadow: &Self::Shadow, range: Range<usize>) -> MResult<()>` to
   `Machine` and call it instead (Plain: `Ok(())`). Until then `check` keeps the failure of
   `Shadow::require_init` in a thread-local slot that `run_test` reads when the run stops.
3. Order around frames, as Miri: `call` before the arguments are copied into the callee's
   locals (so `FnEntry` retags protect in the new frame), and `ret` *before* the return value is
   copied into the caller's destination (`before_stack_pop`: protectors end first).
4. In-place arguments and the return place (Miri `protect_in_place_function_argument`):
   `fn protect(&mut self, place: Ptr<Self::Prov>, size: u64) -> MResult<()>` for every
   `Operand::Move` argument and the return place, after `call`; the checker retags it with a
   strong protector and makes it uninitialized. `Checker::protect` implements it.
5. `cmpxchg`: the evaluator compares itself first, then on success `read` and `write` with
   `Access::Atomic { ord: success, rmw: true }`; on failure one `read` with
   `Access::Atomic { ord: failure, rmw: false }` and no write.
6. GC soundness: no `Shadow` with provenance may live across a `step` outside memory (the
   checker collects the tags it keeps from memory only, at `step`). If the evaluator does keep
   one, it needs `fn live_tags(&self, out: &mut dyn FnMut(Self::Prov))`.
7. Atomic orderings of the suffix intrinsics (`atomic_load_acquire`) and of the const-generic
   ones (`atomic_load::<T, {AtomicOrdering::Acquire}>`) both map to `AtomicOrd`; the tests use
   the suffix form, as minicore has no `AtomicOrdering`.

### C4. Forward to db-ty (hir-def lang items)

`UnsafeUnpin` (`sym::unsafe_unpin`, trait), `GlobalAlloc` (`sym::global_alloc_ty`, struct) and
`MaybeDangling` (`sym::maybe_dangling`, struct) in `crates/hir-def/src/lang_item.rs`: Tree
Borrows' `NewPermission::new` asks `is_unpin && is_unsafe_unpin`, `tb_retag_ptr_value` retags a
`Box` only of the global allocator, and validity skips dereferenceability inside
`MaybeDangling`. Until they land `check::valid` uses `Unpin` alone, takes a `Box` whose
allocator is `alloc`'s `Global` (by crate and name) as global, and has no `MaybeDangling`.

## From safety-model

> applied: M1 2, `"cfg(blaze_check)"` in the root `unexpected_cfgs` `check-cfg`. M1 1 (`blaze-check` workspace dependency) goes in once `crates/blaze-check/` exists (a missing path stops the workspace loading); `blaze-check` is a lock addition then. > forwarded to coordinator: M1 3 (`crates.md`).

### M1. A new crate `crates/blaze-check/` for the model checker (owner: safety-model)

- What: the model checker moves out of `crates/blaze-graph/src/check/` into its own crate
  `crates/blaze-check/` (`Cargo.toml`, `src/lib.rs` and its modules), owned by `safety-model`.
  Nothing goes in `blaze-graph`: no `pub mod check;` is needed there.
- Why: the structures under test must use the shim atomics in model builds through a
  `[target.'cfg(blaze_check)'.dependencies]` dependency on the checker. `blaze-graph` is about to
  depend on `blaze-reclaim` (lane concurrency asks core for `blaze_reclaim::enter()` in
  `pool::Local::run`), and `blaze-vfs` (under `blaze-query`) already does; a `blaze-reclaim` →
  `blaze-graph` dependency is then a cycle, which cargo refuses even under a `cfg` (resolution is
  platform-independent). A leaf crate every one of `blaze-ids`, `intern`, `blaze-query`,
  `blaze-cas`, `blaze-graph`, `blaze-reclaim` can depend on has neither the cycle nor a second
  copy of any type. Design: `docs/v2/plan/lanes/safety/model.md`.
- Its dependencies: `blaze-core` (the simulation driver's `sim::Rng`, so a model's seed is a
  `Sim`'s), `libc` on Unix (stacks), `windows-sys` on Windows (fibers; feature
  `Win32_System_Threading`, already in the workspace list). Constraint to keep: `blaze-core` never
  depends on a crate with model tests.
- Coordinator, root `Cargo.toml`:
  1. `[workspace.dependencies]`: `blaze-check = { path = "crates/blaze-check" }` beside the
     other v2 crates (`crates/*` already makes it a member).
  2. `[workspace.lints.rust]` `unexpected_cfgs` `check-cfg`: add `'cfg(blaze_check)'`.
  3. `docs/v2/plan/reference/crates.md`: a row for `blaze-check` (safety-model) and the
     safety paths line gains `crates/blaze-check/`.
- `Cargo.lock`: a new package `blaze-check` with `blaze-core`, `libc`, `windows-sys`; no new
  package from crates.io.

## From speed-backend-intrinsics (2026-10-29): `unsafe.md` in its paths, and a parser for safety-fuzz

No `unsafe` in `crates/hir-ty-codegen/{asm,x86,neon,sve,fma,avx512,f16_f128,wasm,wasm_object,
wasm_simd}.rs`, `asm/` or `wasm/`. Each of those modules now says `#![forbid(unsafe_code)]` (the
crate as a whole has unsafe in `llvm*.rs`, `backend.rs`, `fncache/` and `mod.rs`, so the crate
attribute is not mine to add). The SIMD lowerings added this pass emit Cranelift vector ops in
the generated code; they are not `unsafe` Rust in blaze.

One parser of untrusted bytes, safe code with no zero-copy view (the bytes are copied out of
the store after its CRC32 check):

- `asm::Assembled::from_bytes(&[u8]) -> Option<Assembled>`: an assembled block as the store
  keeps it (`u32` code length, code, 21-byte relocation records). A value that passes the
  store's checksum and does not parse is now an error naming its key, not a silent
  reassembly. Model: `to_bytes`, with `asm::tests::kept_code_reads_back_and_short_bytes_are_refused`
  (round trip over seeded values, any one byte removed refused). Fuzz target: arbitrary bytes
  must give `None` or a value whose `to_bytes` is those bytes, with no panic and no allocation
  past the input's length.

Not untrusted: `asm.rs`'s `object::File::parse` of the assembler's own output (clang run by
blaze, or LLVM in process) and `wasm_object.rs`/`wasm.rs` reading objects the code generator
wrote in this process.

> forwarded to safety: the `Assembled::from_bytes` fuzz target is safety-fuzz's; the rest is a report, nothing asked of the coordinator.

## From speed-link-edge-macho-dylib (2026-10-29): two parsers of untrusted bytes, for safety-fuzz

There is no `unsafe` in `crates/blaze-link/src/macho/tbd.rs` or `macho/dylib.rs`, and both now
say `#![forbid(unsafe_code)]`. The crate has unsafe in `elf.rs`, `macho.rs`, `macho/sha256.rs`,
`macho/archive.rs` and `macho/input.rs`, so the crate attribute is not mine to add. Neither
module makes a zero-copy view of its own: `dylib.rs` reads through `object`'s bounds-checked
readers, and `tbd.rs` keeps `u32` spans into the `&str` it was given. The input is a file the
link names (the SDK's or the user's), so it is untrusted. Both are safe code, so the targets are
for panics, hangs and memory that grows faster than the input. Each has a safe reference in its
tests to compare against:

- `tbd::read(name, text, arch, platform) -> Result<Shared, String>`: YAML text-based stubs.
  - Fuzz target: arbitrary UTF-8, for each arch in {arm64, x86_64} and platform in {1, 2, 6}.
  - Model: `tbd::tests::reference::read`, the previous reader kept whole. The result or the
    error string must be equal. The only allowed difference is where the reference overflows
    its stack: nesting deeper than 256, which the new reader refuses with an error.
  - Seed corpus: the `generate` function in `tbd::tests`, the SDK's `libSystem.B.tbd`, and
    `edges_match_reference`'s texts.
  - Bounds: the tape is at most one 12-byte node per input byte plus the scratch buffer, which
    holds at most one copy of the text; text of 2 GiB or more is refused.
- `dylib::read(name, bytes, arch) -> Result<Shared, String>`: thin Mach-O dylibs.
  - Fuzz target: arbitrary bytes. The useful part is the export trie, so also mutate only the
    `LC_DYLD_EXPORTS_TRIE` or `LC_DYLD_INFO` range of a valid dylib.
  - Model: `dylib::tests::reference_trie_exports`, the previous walk. The exports or the error
    must be equal, as `trie_walk_matches_reference` checks.
  - Seed corpus: `random_info` and `random_node` in `dylib::tests`.
  - Bounds: the walk visits each trie byte at most once (a bitset), so a cycle is an error and
    never a hang, and the edge stack is at most the trie's length.

Neither needs anything from safety-audit, `--ub` or the model checker. They need only these
fuzz targets, which belong in `fuzz/` (lane safety's).

> forwarded to safety: fuzz targets for safety-fuzz.

## From format-hir-tree (2026-10-29): `unsafe.md` in its paths, and a parser for safety-fuzz

There is no `unsafe` left in `crates/hir-def/src/item_tree.rs` or `item_tree/`.
- The six id newtypes (`ModItemId`, `RawVisibilityId`, `NameId`, `PathId`, `AliasId`,
  `UseTreeId`) used to have a hand-written `unsafe impl Record`. They are now declared with
  `blaze_format::record!`, so the one `unsafe impl` is `blaze_format`'s.
- `decode` used to call `SyntaxContext::from_u32` itself. It now calls a safe
  `crate::hir::persist::syntax_context`, so the trust in persisted contexts is in one place.
  That function is asked of format-hir in `/tmp/blaze-lanes/v2-format-hir-tree.md`.

`hir-def` has unsafe in other modules, so `#![forbid(unsafe_code)]` on the crate is not mine to
add. A module-level `forbid` would reject `record!`'s expansion.

One parser of untrusted bytes, the persisted item tree:
`<ItemTree as Persist>::read(buffer)` then `ItemTreeRef::decode() -> Result<ItemTree, FormatError>`.
- Zero-copy: `read` gives `ItemTreeRef::top_level(): &[ModItemId]` in place. That view is
  bounds-checked by `Slice::follow`, but its ids are only checked by `decode`. A bad id panics
  on use; it is never UB. The store's hash check of the buffer comes before `read`, and that
  check is the store's.
- Everything else is copied out one column at a time, and `decode` checks every id, range and
  kind, as listed in the report.

The fuzz target, the model, the seed corpus and the bounds:
- **Fuzz target.** For arbitrary bytes, `read` then `decode` must give `Err`, or a tree whose
  write is those bytes again.
  - It must not panic, hang or allocate more than a small multiple of the input.
  - Also mutate only the column bytes of a valid tree, since that is where `check` works.
- **Model.** `persist::tests::assert_round_trip`: write, read, decode, compare, and write again
  to the same bytes.
- **Seed corpus.** The fixtures in `item_tree/persist.rs`'s tests and every fixture in
  `item_tree/tests.rs`.
- **Bounds.** Each column is copied from a bounds-checked slice. The tables are read by
  `Read::seq`, which doesn't preallocate, and every element reads at least one word, so a
  corrupted count fails at the end of the input.
- **Known hole.** An ast id's kind isn't checked, because span keeps `kind_typed` private. Such
  a tree decodes, then trips a debug assertion in `ItemTree::index`. That is a panic, not UB,
  and the fix is asked of span in `v2/requests/format.md` ("## format-hir-tree").

## From speed-backend-code (2026-10-29): `unsafe.md` in its paths

No `unsafe` in `crates/hir-ty-codegen/{fragment,promote,overlap,copies,home,settled,places,
float_pair,vtable,type_id}.rs`. Each now says `#![forbid(unsafe_code)]` (`places.rs` has
`#[salsa::tracked]` fns, whose expansion the lint does not report, as safety-audit-linkgen
found for hir-ty-mir). This pass's speed-ups there (16-byte relocation rows, `HashTable`
request tables, dense entity maps and bitsets in `promote`/`overlap`) are safe code, each
with a differential test against the version it replaced.

`fncache.rs` is not forbidden: its child `fncache/dispatch.rs` has the crate's only unsafe in
these paths, two `ModuleIdLt::to_static` calls in `why_not`. Both now carry a `// SAFETY:`
naming the invariant (the `ModuleId` does not outlive `db`'s revision), where it is upheld
(`outer` is a local; `why_not` returns `&'static str`, `owners` returns `DefWithBodyId`s) and
what breaks otherwise. Making them safe needs a safe walk to a block's outermost module in
hir-def (`ModuleIdLt::outermost(db) -> ModuleIdLt<'db>`), which is the front end's, not
asked here.

No parser of untrusted bytes in these paths.

> for safety-audit-linkgen-cg: the two `dispatch.rs` blocks are documented; nothing else to find in the files above.

## From speed-runtime-vfs (2026-10-29): `crates/blaze-vfs` under unsafe.md, for safety-audit-edge and safety-model

- Unsafe is only in `src/store.rs` (32 blocks, each with `// SAFETY:` and one unsafe op; every
  `unsafe fn` is private with `# Safety`). Its API is safe: every read is checked against a
  column length published with release, every push goes through the one `Writer` (`&mut
  self`, never cloned) or a `Building` nobody else holds, and a reader reaches a table only
  through a `Snapshot` holding its `blaze_reclaim` task. `lib.rs` says `#![deny(unsafe_code)]`
  with `#[allow(unsafe_code)] mod store;` (`forbid` can't be lifted for one module).
- Differential tests against safe references are in `src/store/tests.rs` (`Vec`, `FxHashMap`,
  `Vec<String>`), plus the crate's model tests in `src/tests.rs`.
- Model checking: `src/sync.rs` is the switch from model.md; `model_a_node_reads_whole_or_not_at_all`
  (`#[cfg(blaze_check)]`) runs a writer adding a file and its directory against a reader. Its
  manifest line is requested in v2/requests/speed.md until `blaze-check` is in the workspace.
  Row for model.md's table: `blaze-vfs` | `store.rs` columns, index levels, table swap | vfs.
- Untrusted bytes: the journal (`src/journal.rs`) is parsed with checked slicing after its
  checksum; nothing is viewed zero-copy from it (a file's bytes are copied into an `Arc`).
  safety-fuzz-vfs's entry point is `Vfs::recover(file, tags, bytes)`.

## From safety-audit: coordinator

From `safety-audit-coord` (2026-10-29). The coordinator's crates, read block by block against
`docs/v2/plan/design/unsafe.md` and the three lints. Comments, splits and crate-root attributes
only; nothing here changes what code does except where a line says "owner's choice". Line
numbers are as of this audit. Full table: `/tmp/blaze-lanes/v2-safety-audit-coord.md`.

### Crate roots: `#![forbid(unsafe_code)]`

None of these compiles any `unsafe` (every match is a string, a comment or a fixture; no
external macro they use expands to `unsafe`). Add `#![forbid(unsafe_code)]` to every target
root listed:

- `crates/blaze-bench/src/lib.rs`, `crates/blaze-bench/benches/store.rs`
- `crates/blaze-manifest/src/lib.rs`, `examples/config.rs`, `examples/metadata.rs`,
  `examples/profiles/main.rs`, `tests/cargo_features.rs`, `tests/config_get.rs`,
  `tests/config_typed.rs`, `tests/fixtures.rs`, `tests/profiles.rs` (all under
  `crates/blaze-manifest/`; the `.rs` files under `tests/*-fixtures/` and `tests/fixtures/` are
  fixture data, not targets)
- `crates/blaze-metadata/src/lib.rs`, `examples/metadata.rs`, `examples/units.rs`
- `crates/blaze-resolve/src/lib.rs`, `examples/metadata_resolve.rs`, `tests/features.rs`,
  `tests/lockfile.rs`, `tests/metadata.rs`, `tests/resolver.rs`, `tests/workspace_resolve.rs`
- `crates/blaze-source/src/lib.rs`, `crates/cfg/src/lib.rs`, `crates/edition/src/lib.rs`,
  `crates/macros/src/lib.rs`, `crates/test-fixture/src/lib.rs`, `crates/toolchain/src/lib.rs`,
  `crates/blaze-new/src/lib.rs`, `crates/blaze-lints/src/lib.rs`,
  `crates/blaze-cache-macros/src/lib.rs` (the brief's list: confirmed)
- `crates/blaze-doc/src/lib.rs` (its only `unsafe` is in fixture strings in `src/tests.rs` and in
  rendered text)
- `crates/test-utils/src/lib.rs` (its `unsafe` is all in `src/minicore.rs`, which is
  `include_str!`'d at `src/fixture.rs:368` and never a module, and in `bench_data/`)
- Test and example roots of crates whose library keeps `unsafe` (none of these has any):
  `crates/blaze-registry/examples/{fetch,query}.rs`,
  `crates/blaze-registry/tests/{auth,fetch,git,git_index,query}.rs`,
  `crates/blaze-fmt/examples/{cargo-fmt,rustfmt}.rs`, `crates/blaze-fmt/tests/format.rs`,
  `crates/blaze-cache/tests/plain.rs`, `crates/blaze-wasm/src/main.rs`,
  `crates/blaze-wasm/core-macro/src/lib.rs` and `crates/blaze-wasm/core-macro/build.rs` (the
  `dioxus-core-macro` 0.7.10 sources it `include!`s have no `unsafe`),
  `crates/blaze-wasm/web/dioxus/app/src/main.rs` (owner's choice: the page also shows this file
  as the demo's source).

No change needed for `crates/hir-ide/` and `crates/ide-db/`: each has one block, and both
SAFETY comments were written during this audit (`hir-ide/src/const_macro.rs:102`,
`ide-db/src/lib.rs:106`) and are good. hir-ide's ~300 other `unsafe` matches are all in `r#"`
fixture strings under `src/tests/`, plus `display.rs`'s rendered text.

### stdx

- `crates/stdx/src/anymap.rs:120` block (inside `.map(|any| unsafe { .. })`; the comment goes on the line above 120): no SAFETY. Proposed: `` // SAFETY: `raw` is written only by `VacantEntry::insert`, which stores `T::into_box` under `TypeId::of::<T>()`, and for the three `dyn Any` maps `into_box` is the blanket `Box::new`, so the box under this key holds a `T`; a box of another type here would be read as a `T`. ``
- `crates/stdx/src/anymap.rs:175` block: no SAFETY. Proposed: `` // SAFETY: this entry is the one for `TypeId::of::<V>()`, and `VacantEntry::insert` is the only writer, so its box holds a `V` (see `Map::get`); otherwise the reference would be to another type. ``
- `crates/stdx/src/anymap.rs:184` block: no SAFETY. Proposed: `` // SAFETY: the box just inserted is `value.into_box()`, a `V` (the blanket `IntoBox` is `Box::new`), so downcasting it to `V` is to its own type. ``
- `crates/stdx/src/anymap.rs:273` block (in `implement!`'s body, so the comment goes inside the macro): no SAFETY. Proposed: `` // SAFETY: the caller promises (this method's `# Safety`) that `T` is the concrete type behind `self`, so `self`'s data pointer is a live, aligned `T` borrowed for `self`'s lifetime; `cast` drops only the vtable. Another `T` would make this a reference to a value of the wrong type. ``
- `crates/stdx/src/anymap.rs:278` block (same macro): no SAFETY. Proposed: as :273, with "uniquely borrowed" for "borrowed".
- `crates/stdx/src/anymap.rs:272`, `:277` unsafe fns: impls of `Downcast`'s methods; they inherit its `# Safety` (`:243-246`, `:251-253`). No doc wanted.
- `crates/stdx/src/anymap.rs:258` MAY BE UNSOUND (latent, nothing in the tree reaches it): `Map<A>` trusts that `IntoBox<A>::into_box` boxes `Self`, but `IntoBox` and `Downcast` are safe public traits. A crate that defines its own `A`, gives it the usual `Downcast` cast, and implements `IntoBox<A> for u32` returning a box of something else gets UB from `Map::get::<u32>` without writing `unsafe` in the lying impl. The three `dyn Any` maps are sound (coherence leaves only the blanket impl), and the one user, `hir-def/src/dyn_map.rs`, uses `Map<dyn Any>`. Owner's choice: make `IntoBox` an `unsafe trait`, or seal it.
- `crates/stdx/src/lib.rs:345` block: comment says too little ("repr transparent, except on WASM"). Proposed: `` // SAFETY: `JodChild` is `repr(transparent)` over `Child` wherever this fn is compiled (its `cfg` is the struct's `cfg_attr`), so the two have one layout; the transmute moves the `Child` out without running `JodChild::drop`, which would kill it. Without the `repr` the field's place in `JodChild` would not be guaranteed. ``
- `crates/stdx/src/process.rs:93` block: 2 ops (two `libc::fcntl`). Split: `unsafe { libc::fcntl(out_pipe.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };` (SAFETY: `` `out_pipe` owns this open descriptor for the whole call, and `F_SETFL` with an integer flag reads no memory; a closed descriptor would fail with `EBADF`, not touch memory. ``) then `unsafe { libc::fcntl(err_pipe.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };` (SAFETY: `` `err_pipe` owns this open descriptor for the whole call, and `F_SETFL` with an integer flag reads no memory. ``).
- `crates/stdx/src/process.rs:103` block: no SAFETY. Proposed: `` // SAFETY: `pollfd` is a C struct of integers (`fd`, `events`, `revents`), for which all-zero bytes are a valid value; a reference or `NonZero` field would make this UB. ``
- `crates/stdx/src/process.rs:113` block: no SAFETY. Proposed: `` // SAFETY: `fds` is a live array of two `pollfd`s and `nfds` is 2 or 1 (it only goes down from 2, once per finished pipe), so `poll` reads and writes only inside `fds`; a larger `nfds` would let it write past the array. ``
- `crates/stdx/src/process.rs:189` block (Windows): no SAFETY and 8 ops (`Pipe::new` at :190 and :191, `read` at :193, :194, :203, :207, `complete` at :201, :205). Split: drop the outer `unsafe {` and wrap each call, keeping `?` outside the block (`unsafe { out_pipe.read() }?;`), with one comment each. `Pipe::new`: `` // SAFETY: `out_pipe` (`err_pipe`) is the parent's end of the child's pipe, an open handle it owns, and it was just added to `port`. `` Each `read`: `` // SAFETY: the `Pipe` stays in this frame, unmoved, until `read2` returns, and its `dst` is touched only after `port` hands back this read's completion (but see MAY BE UNSOUND below). `` Each `complete`: `` // SAFETY: `status` has token 0 (1), the token `out_pipe`'s (`err_pipe`'s) handle was added to `port` with, so it is the completion of that pipe's pending read. `` The block's tail `Ok(())` becomes the fn's.
- `crates/stdx/src/process.rs:217` unsafe fn `Pipe::new`: no `# Safety`. Its body has no obligation the types don't already give (`into_raw_handle` hands over an open, owned handle), so owner's choice: make it safe. If kept: `` /// # Safety\n/// `p`'s handle must be open, and owned by `p`. ``
- `crates/stdx/src/process.rs:218` block: no SAFETY. Proposed: `` // SAFETY: `into_raw_handle` gives up `p`'s ownership of an open handle, which the `NamedPipe` now owns and closes; a handle still owned elsewhere would be closed twice. ``
- `crates/stdx/src/process.rs:222` unsafe fn `Pipe::read`: no `# Safety`. Proposed: `` /// # Safety\n/// Until this read's completion has been passed to [`Pipe::complete`], `self` must not move or be dropped, and `self.dst` must not be reallocated, freed or read past its length: the kernel writes into `self.overlapped` and `self.dst`'s spare capacity. ``
- `crates/stdx/src/process.rs:223` block: no SAFETY. Proposed (once `slice_to_end` is fixed, see below): `` // SAFETY: `self.dst` is a live `Vec` that nothing else borrows while the slice lives. ``
- `crates/stdx/src/process.rs:224` block: no SAFETY. Proposed: `` // SAFETY: `dst` and `self.overlapped` stay valid and unmoved until this read completes, which this fn's `# Safety` puts on its caller; freed early, the kernel would write into freed memory. ``
- `crates/stdx/src/process.rs:237` unsafe fn `Pipe::complete`: no `# Safety`. Proposed: `` /// # Safety\n/// `status` must be the completion of the read last started by [`Pipe::read`] on `self`. ``
- `crates/stdx/src/process.rs:239` block: no SAFETY. Proposed: `` // SAFETY: by this fn's `# Safety`, the kernel wrote `bytes_transferred` bytes right after `prev`, inside `dst`'s capacity (the read was given only the spare capacity), so they are initialized; a wrong `status` would expose uninitialized bytes. ``
- `crates/stdx/src/process.rs:246` unsafe fn `slice_to_end`: no `# Safety`. Proposed (with the fix below): `` /// # Safety\n/// `v` must not be used while the returned slice lives. ``
- `crates/stdx/src/process.rs:253` block: no SAFETY. Proposed: `` // SAFETY: `len <= capacity`, so the offset stays inside `v`'s allocation, at most one past its end. ``
- `crates/stdx/src/process.rs:255` MAY BE UNSOUND: `slice::from_raw_parts_mut(data, len)` makes a `&mut [u8]` over `v`'s spare capacity, which `reserve` never initializes, but `from_raw_parts_mut` requires `len` initialized values. No comment can honestly say it holds. Fix (owner's, behavior-changing): zero the spare capacity first (`v.spare_capacity_mut().fill(MaybeUninit::new(0))`) and then build the slice, or hand miow a raw pointer. Windows only.
- `crates/stdx/src/process.rs:189` MAY BE UNSOUND: if `port.get_many(..)?` or one pipe's `read()?` returns `Err` while the other pipe's overlapped read is pending, `read2` returns and drops that `Pipe` (its `OVERLAPPED`) and then `out`/`err`, while the kernel may still complete the read into them. Closing the handle cancels the read, but Windows requires the buffer and `OVERLAPPED` to stay valid until the cancellation completes. cargo's `read2` has the same code. Fix (owner's): on the error path, `CancelIoEx` each pending pipe and wait for its completion before returning. Windows only.
- `crates/stdx/src/tempfile.rs:146` block: no SAFETY. Proposed: `` // SAFETY: `original_path` is a `CString` that lives to the end of this `if`, so `unlink` reads a NUL-terminated name in live memory; a dangling pointer would have it read freed memory. ``
- `crates/stdx/src/time.rs:16`: not an unsafe fn (an `unsafe extern` block declaring `safe fn now`); no finding.
- `crates/stdx/src/thread/intent.rs:182` block: no SAFETY. Proposed: `` // SAFETY: `c` is one of the four `qos_class_t` values above and relative priority 0 is in range; the call takes no pointer. ``
- `crates/stdx/src/thread/intent.rs:188` block: 2 ops (`__error()` call, deref). Split: `let errno = unsafe { libc::__error() };` (SAFETY: `` `__error` has no precondition and returns this thread's `errno` slot. ``) then `let errno = unsafe { *errno };` (SAFETY: `` `errno` points to this thread's `errno`, which lives as long as the thread and is aligned; it is a plain `c_int`, so the read is of an initialized value. ``).
- `crates/stdx/src/thread/intent.rs:220` block: no SAFETY. Proposed: `` // SAFETY: `pthread_self` has no precondition and cannot fail. ``
- `crates/stdx/src/thread/intent.rs:222` block: no SAFETY. Proposed: `` // SAFETY: `current_thread` is this thread, `&mut qos_class_raw` is a live `qos_class_t` for the call, and `relative_priority` may be null (libpthread only writes it when it isn't). ``
- `crates/stdx/src/thread/intent.rs:236` block: 2 ops. Split as :188.
- `crates/stdx/src/thread/pool.rs:141` MAY BE UNSOUND: the transmute stretches a `'scope` job to `'static`, which is sound only if every job finishes before `'scope` ends. `Pool::scoped` waits (`scope.wg.wait()`, `:104`) only when `f` returns: if `f` panics after spawning, unwinding skips the wait, the caller's borrowed data is dropped, and the job can still run on a worker and use it (use after free). There is no caller of `Pool::scoped` in the tree today. Fix (owner's, behavior-changing): wait in a drop guard, as `std::thread::scope` does. Then: `` // SAFETY: the job may borrow data for `'scope`; it holds a clone of `wg` until it has run, and `Pool::scoped` waits on `wg` before `'scope` ends, even on unwind, so it never outlives what it borrows. ``

### profile

- `crates/profile/src/google_cpu_profiler.rs:12`: an `extern` block declaration, not an unsafe fn; no finding.
- `crates/profile/src/google_cpu_profiler.rs:32` block: no SAFETY. Proposed: `` // SAFETY: `path` is a `CString` alive across the call, so `ProfilerStart` reads a NUL-terminated name in live memory. ``
- `crates/profile/src/google_cpu_profiler.rs:42` block: no SAFETY. Proposed: `` // SAFETY: `ProfilerStop` takes no arguments; the `ON -> PENDING` transition above means a profile `start` began is running and no other call stops it at the same time. ``
- `crates/profile/src/memory_usage.rs:41` block: no SAFETY. Proposed: `` // SAFETY: `GetCurrentProcess` has no precondition; it returns a pseudo-handle that needs no closing. ``
- `crates/profile/src/memory_usage.rs:45` block: no SAFETY. Proposed: `` // SAFETY: `mem_counters.as_mut_ptr()` is a live, writable `PROCESS_MEMORY_COUNTERS` and `cb` is its size, so the call writes only inside it; a larger `cb` would let it write past it. ``
- `crates/profile/src/memory_usage.rs:48` block: no SAFETY. Proposed: `` // SAFETY: `ret != 0` was asserted, so `GetProcessMemoryInfo` filled `mem_counters`; read before that, it would be uninitialized. ``
- `crates/profile/src/memory_usage.rs:69` block: no SAFETY. Proposed: `` // SAFETY: `RTLD_DEFAULT` is a valid pseudo-handle and `c"mallinfo2"` a NUL-terminated static name; `dlsym` only looks the symbol up. ``
- `crates/profile/src/memory_usage.rs:76` block: no SAFETY. Proposed: `` // SAFETY: glibc's `mallinfo` takes no arguments and returns its struct by value; this fn is compiled only for `linux`/`gnu`. ``
- `crates/profile/src/memory_usage.rs:80` block: no SAFETY. Proposed: `` // SAFETY: `mallinfo2` is the non-null address `dlsym` gave for glibc's `mallinfo2` (`MALLINFO2` holds only 1 for "not looked up" or `dlsym`'s result, and 0 is handled above), whose C type `struct mallinfo2 mallinfo2(void)` is `extern "C" fn() -> libc::mallinfo2`; another address would be called with the wrong signature. ``

### paths

- `crates/paths/src/lib.rs:211` block: no SAFETY. Proposed: `` // SAFETY: `AbsPath` is `#[repr(transparent)]` over `Utf8Path` (`:158`), so the cast keeps `path`'s address, length and lifetime; without the `repr` the two unsized layouts would not be guaranteed to match. ``
- `crates/paths/src/lib.rs:394` block: no SAFETY. Proposed: as :211, for `RelPath` (`:376`).

### rustc_abi

Not under `[lints] workspace = true` (it is rustc's `compiler/rustc_abi/src` unchanged, per its
`Cargo.toml`). Its only `unsafe` is `impl Step for Size`, compiled only with feature `nightly`,
which blaze never enables (it needs the compiler's own crates).

- `crates/rustc_abi/src/lib.rs:1073`, `:1094` unsafe fns `forward_unchecked`, `backward_unchecked`: impls of `core::iter::Step`'s methods; they inherit its `# Safety`. No doc wanted (clippy's `missing_safety_doc` skips trait impls, and it is not one of the three lints).
- `crates/rustc_abi/src/lib.rs:1074` block: no SAFETY. Proposed: `` // SAFETY: the caller upholds `Step::forward_unchecked`'s contract for `Size`, that `count` steps from `start` don't overflow; a `Size` steps as its bytes, a `u64` (see `forward`), so the same holds for `u64::forward_unchecked`, whose overflow would be UB. ``
- `crates/rustc_abi/src/lib.rs:1095` block: no SAFETY. Proposed: as :1074, "don't go below zero", for `backward_unchecked`.
- Crate root: owner's choice, because any edit makes the copy differ from upstream: `#![cfg_attr(not(feature = "nightly"), forbid(unsafe_code))]` at the top of `src/lib.rs`, true to the rule, since without `nightly` nothing is unsafe. The alternative is to keep the copy verbatim and record that it is exempt.

### blaze-fuzz

- `crates/blaze-fuzz/src/lib.rs:114`, `:120`, `:125` unsafe fns: `GlobalAlloc` impl methods; they inherit its `# Safety`. No doc wanted.
- `crates/blaze-fuzz/src/lib.rs:113` unsafe impl: comment is thin. Proposed: `` // SAFETY: every method passes its request to `System` unchanged, so `Counting` keeps `GlobalAlloc`'s contract as `System` does; `saw` only sets a const-initialized thread-local `Cell` through `try_with`, so it neither allocates nor panics (either would re-enter or unwind out of the allocator). ``
- `crates/blaze-fuzz/src/lib.rs:117` block: thin. Proposed: `` // SAFETY: `layout` is the caller's, who upholds `GlobalAlloc::alloc`'s contract (non-zero size); `System.alloc` gets it unchanged. ``
- `crates/blaze-fuzz/src/lib.rs:122` block: thin. Proposed: `` // SAFETY: the caller promises `at` was allocated by this allocator with `layout`; every allocation here is `System`'s, so `System.dealloc` gets its own pointer and layout. ``
- `crates/blaze-fuzz/src/lib.rs:128` block: thin. Proposed: `` // SAFETY: as in `dealloc`, `at` and `layout` are a live `System` allocation, and the caller promises `size` is non-zero and fits `isize` once rounded up to `layout.align()`, as `System.realloc` requires. ``

### blaze-registry

- `crates/blaze-registry/src/download.rs:372` block: 2 ops (two `libc::umask`). Split, inside the same closure: `let mask = unsafe { libc::umask(0o022) };` (SAFETY: `` `umask` only swaps the process's file mode mask; it reads no memory and cannot fail. ``) then `unsafe { libc::umask(mask) };` (SAFETY: `` puts back the mask the call above read, at once, before any unpacking; `umask` touches no memory. ``) then `u32::from(mask)`.

### blaze-fmt

- `crates/blaze-fmt/src/cargo_fmt.rs:622` block: 3 ops (`libc::dup`, `OwnedFd::from_raw_fd`, `libc::dup2`). Split, same order: `let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };` (SAFETY: `` `dup` of standard output, open for the whole program; it reads no memory, and failure is the `< 0` checked next. ``); the `if saved < 0 { return None; }`; `let saved = unsafe { OwnedFd::from_raw_fd(saved) };` (SAFETY: `` `saved` is a new descriptor from `dup`, checked non-negative, that nothing else owns, so `OwnedFd` may close it; another owner would see it closed twice. ``); `if unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) } < 0 { return None; }` (SAFETY: `` `null` is an open file of ours for the call; std's I/O safety allows replacing descriptor 1 with `dup2` (not closing it), and `saved` keeps the original for `drop` to restore. ``); `Some(StdoutToNull { saved })`.
- `crates/blaze-fmt/src/cargo_fmt.rs:648` block: comment doesn't name the invariant. Proposed: `` // SAFETY: `self.saved` is the open descriptor `new` duplicated from standard output, owned by `self` past this call, so `dup2` copies a live descriptor back onto 1. ``

### blaze-cache

Not under `[lints] workspace = true` (a published crate). Audited anyway.

- `crates/blaze-cache/src/lib.rs:74` block: 2 ops (`slice::from_raw_parts`, `str::from_utf8_unchecked`). Split: `let bytes: &'static [u8] = unsafe { std::slice::from_raw_parts(self.text, self.len) };` (SAFETY: `` a `Code` with `len != 0` is made only by blaze, which writes the address and length of a `&'static str` of the binary into `text` and `len`; safe code can't make one (`text` and `len` are private, and `UNKNOWN` has `len` 0). A wrong pair would read outside that string. ``) then `unsafe { std::str::from_utf8_unchecked(bytes) }` (SAFETY: `` `bytes` are that `&'static str`'s, so UTF-8. ``).

### blaze-wasm

Not under `[lints] workspace = true` (its own workspace, as are `web/dioxus/`'s). Audited anyway.
The brief's 27 blocks and 5 fns are all in `web/demos/*.rs`: demo programs that
`web/dioxus/playground/src/lib.rs:216-248` `include_str!`s as source for blaze to compile in the
page. They are no crate's code and no lint sees them; nothing to change (owner's choice to give
them SAFETY comments, since the page shows them as examples).

- `crates/blaze-wasm/src/exports.rs:120` block: 2 ops (two `argument` calls). Split, same order: `let path = unsafe { argument(path, path_len) };` then `let text = unsafe { argument(text, text_len) };`, each with `` // SAFETY: this fn's `# Safety` is [`argument`]'s, for this pointer and length. ``
- `crates/blaze-wasm/src/exports.rs:30` block: thin. Proposed: `` // SAFETY: `text` lives through the call, and the page's `blaze.log` reads `len` bytes at `ptr` in this module's memory and keeps nothing. ``
- `crates/blaze-wasm/src/exports.rs:54`, `:75` blocks: "the caller's promise" says too little. Proposed: `` // SAFETY: this fn's `# Safety` is [`argument`]'s, so `ptr` and `len` are what `argument` asks. ``
- `crates/blaze-wasm/src/exports.rs:43` block: fine; stronger: `` // SAFETY: by this fn's `# Safety`, `ptr` is a `Vec<u8>` that `blaze_alloc` leaked with capacity `len` (`with_capacity` gives exactly `len` for `u8`), and the page wrote all `len` bytes; another capacity would be freed with the wrong layout. ``
- `crates/blaze-wasm/web/dioxus/playground/src/lib.rs:46` block: comment lacks what breaks. Append: `` A fn pointer on wasm is a table index; another index would be called as `fn() -> Element`. ``
- The `#[unsafe(no_mangle)]` exports and the `unsafe extern` imports need no comment (all `blaze_`-prefixed or explained, as `stderr`/`isprint` at `playground/src/lib.rs:383`).

## From safety-fuzz

### F1. Coordinator: a CI job that fuzzes every target for its budget after merge (`.github/`)

What: a job in the release train (`.github/workflows/train.yaml`, which runs after merge; or a
workflow of its own on `push` to `main` if the train stays by-hand only), on Linux x86-64 (and
macOS arm64 once codegen's ASan links there), with one matrix entry per `[[bin]]` of
`fuzz/Cargo.toml` (list them with
`python3 -c 'import tomllib;print("\n".join(b["name"] for b in tomllib.load(open("fuzz/Cargo.toml","rb"))["bin"]))'`).
Each entry:

1. builds the target with blaze, never cargo or rustc (AGENTS.md no-rustc, blaze-builds-blaze):
   `RUSTFLAGS="-Zsanitizer=address -Csanitizer-coverage-level=3 -Zsanitizer-coverage=inline-8bit-counters,pc-table,trace-cmp -Cdebug-assertions" blaze build --release -p blaze-fuzzer --bin <target>`;
2. runs `target/release/<target> fuzz` (no `--seconds`: each target's `Target::budget`, 600 s
   unless the target says) and fails the job on a nonzero exit (1: a finding; 2: the build lacks
   a flag, which is the job's bug, never skipped);
3. always uploads `fuzz/out/<target>/` (the corpus it grew, and any `crash-*.hex` or
   `timeout-*.hex`) as an artifact `fuzz-<target>-<run>`, and caches `fuzz/out/<target>/corpus/`
   between runs (key `fuzz-<target>-<sha>`, restore prefix `fuzz-<target>-`) so each run starts
   where the last stopped;
4. on a finding, runs `target/release/<target> minimize fuzz/out/<target>/crash-*.hex --seconds 300`
   and adds the minimized input to the artifact and the step summary (its hex, and the replay
   command `<target> run <file>`).

The merge gate (blaze.yaml) needs nothing new: each target's seed corpus runs there as a plain
`#[test]` of the `blaze-fuzzer` package with the rest of `blaze test`.
Why: docs/v2/plan/design/unsafe.md "Done when": every untrusted-bytes parser has a fuzz target
that has run its time budget in CI with no finding. `.github/` is the coordinator's.

### F2. Codegen (`v2/requests/codegen.md`, adds to S2): what the fuzz engine relies on

The engine (`fuzz/src/raw.rs`) is itself instrumented, like every crate of a fuzz build. For it
to work, S2's instrumentation must also:

1. Run SanitizerCoverage before AddressSanitizer in the pipeline (clang's order in
   `addSanitizers`), or mark ASan's own shadow checks `!nosanitize` so SanitizerCoverage skips
   them. Otherwise each shadow check in a `__sanitizer_cov_trace_cmp*` callback is a compare
   that calls the callback again, without end. The callbacks are also `#[sanitize(address =
   "off")]` (feature `sanitize`), which S2 already says codegen honors.
2. Give `core::intrinsics::return_address()` as LLVM's `llvm.returnaddress(0)` (rustc does);
   the engine stops with an error naming it when it is null, since the value profile needs the
   comparison's site.
3. Emit with `inline-8bit-counters` the PC table (`pc-table`: `__sanitizer_cov_pcs_init`, each
   function's entry block flagged 1 at the function's address) and with `trace-cmp` the
   `__sanitizer_cov_trace_{,const_}cmp{1,2,4,8}` and `__sanitizer_cov_trace_switch` calls, exactly
   as LLVM's `SanitizerCoveragePass` does; the engine defines those symbols and
   `__sanitizer_cov_8bit_counters_init`, `__sanitizer_cov_trace_div{4,8}`,
   `__sanitizer_cov_trace_gep`, `__sanitizer_cov_trace_pc_indir`, `__sanitizer_weak_hook_memcmp`,
   `__asan_default_options` and `__asan_on_error` (no libFuzzer is linked).
4. Apply `RUSTFLAGS`' sanitizer flags to every crate of the build, the target binary's
   dependencies included (cargo's behavior), so the parsers under test are instrumented.

Why: fuzz/README.md; without 1 the fuzz build recurses, without 2-3 it stops with an error.

### F3. Cargo (`v2/requests/cargo.md`, adds to item 2 of "From safety")

Pass `-Zsanitizer-coverage=...` and `-Csanitizer-coverage-level=N` through to codegen (with
`-Zsanitizer`), and `cfg(sanitize = "address")` to the crates, once S2 lands.

## From safety-race

`crate::race` is in `crates/hir-ty-ub/src/race.rs` (+ `src/race/`). Its API follows the brief
except where the brief's signatures could not carry what Miri needs; safety-check should call
these (read the doc comment at the top of `race.rs` for the per-event protocol):

- `Races::store_value(alloc, range, ord, bytes, old: Option<&[u8]>)`: the extra `old` is the
  location's bytes *before* the store (`None` if not all initialized). Miri reads it
  (`write_scalar_atomic`'s `old_val`) because a location's first atomic store creates its store
  buffer with the previous value as the oldest store; without it a weak load can never read the
  initial value (litmus tests `relaxed`, `initialization_write`, `weak_uninit`).
  **Request (forward to codegen-mir, owner of the evaluator's `Machine`)**: `Machine::atomic_store`
  should pass the old bytes: `fn atomic_store(&mut self, ptr, ord, bytes: &[u8], old: &[u8])`
  (the checker's shadow says which of `old` are initialized).
- `Races::load_value(alloc, range) -> Result<Option<&[u8]>, RaceError>`: no `ord`/`latest`
  (the store is picked in the atomic `read`, which must acquire that store's clock); returns
  an error for a weak load that read an uninitialized older store (Miri `weak_uninit.rs`:
  "using uninitialized data, but this operation requires initialized memory"), and borrows the
  bytes (zero copy; `Machine::atomic_load` copies them into its `Vec`).
- Added, with no Machine hook yet: `Races::yield_active()` (sched_yield / thread::yield_now /
  spin_loop hint: Miri's `yield_active_thread`; `schedule` can't tell a yield from a
  terminator), `Races::failed_cxchg(alloc, range, ord, site)` (a failed compare-exchange reads
  the latest store, not a random one), `Races::rmw(...)` (the brief's), `Races::detach(thread)`,
  `Races::set_thread_name(thread, name)`, `Races::retag(alloc, range, AccessKind, site)` (Tree
  Borrows' implicit retag accesses, Miri reports them as "retag read/write"),
  `Races::spurious_failure()` (compare_exchange_weak, Miri's 80%).
  **Request (forward to codegen-mir)**: `Machine::yield_now(&mut self)`, `Machine::detach(&mut
  self, thread: EvalThread) -> MResult<()>`, and on `Access::Atomic` a way to say "this read is
  a failed compare-exchange" (e.g. `Access::Atomic { ord, rmw: false, cxchg_failed: true }`),
  plus `Machine::thread_name(&mut self, thread, name: &[u8])` for `pthread_setname_np`.
