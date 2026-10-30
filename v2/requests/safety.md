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

## 3. Coordinator: `docs/v2/plan/reference/crates.md`

Add lane `safety` to "Paths each lane owns": `crates/hir-ty-ub/`, `fuzz/`,
`crates/blaze-graph/src/check.rs` and `crates/blaze-graph/src/check/` (taken out of core's
`crates/blaze-graph/`), `docs/v2/plan/lanes/safety/`; and `hir-ty-ub` to the v2 crates table
("the checker of undefined behavior over hir-ty-mir's evaluator: `Checker`, `Config`,
`Outcome`, `run_test`", owner safety). A front-end crate (AGENTS.md semantics-in-front-end).

## 4. Coordinator: `docs/support.md`, once codegen's sanitizers land

The row at line ~345 refuses `sanitizer`, `sanitizer-recover`,
`sanitizer-memory-track-origins` (and the CFI flags) because "blaze's code generator does not
instrument". When codegen's request (v2/requests/codegen.md, "From safety", item S2) lands,
`-Zsanitizer=address`, `thread`, `memory` and `-Zsanitizer-recover`,
`-Zsanitizer-memory-track-origins` and `-Csanitizer-coverage-*` move to supported; what is
still refused (CFI, KCFI, dataflow, if codegen doesn't do them) stays, with that reason.

## 5. Forwarded: what lane safety asked of others

- codegen (`v2/requests/codegen.md`, "From safety"): S1, the evaluator's `Machine` hooks in
  `crates/hir-ty-mir/src/mir/eval*` (codegen-mir owns it); S2, ASan, TSan, MSan and
  SanitizerCoverage in the LLVM back end, with their runtimes linked.
- db-ty (`v2/requests/db-ty.md`, "From safety"): `MirEvalError::MachineStop` in
  `crates/hir-ty/src/mir.rs`.
- cargo (`v2/requests/cargo.md`, "From safety"): `blaze test --ub`.
- core (through this file, from safety-model when it writes it): `pub mod check;` in
  `crates/blaze-graph/src/lib.rs`.
