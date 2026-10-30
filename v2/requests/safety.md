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
