# Patches on rustc's `compiler/rustc_next_trait_solver`

Base: rust-lang/rust `16d1ffac17ad8b739e32483ac2e65fe7e1804977`, nightly-2026-10-27.

`src/` is upstream's `compiler/rustc_next_trait_solver/src` at the base commit with the series
below applied in order (`docs/v2/plan/design/compile-time.md`, change 18).
`tools/v2/solver_sync.py` replaces the base on every toolchain bump and replays the series; it
stops at the first patch that no longer applies, naming it. Each `### NNNN-name` heading is
`patches/NNNN-name.patch`, a diff of `src/` taken with `a/` and `b/` prefixes. To change the
series: edit `src/`, regenerate the patch, and keep this file's headings and the `patches/`
directory one for one.

`Cargo.toml` is blaze's, built as rust-analyzer's `ra-ap-rustc_next_trait_solver` builds the
crate: `nightly` off. `Cargo.toml.orig` is upstream's manifest at the base. The script stops if
upstream's changes, until `Cargo.toml` follows it.

## Series

### 0001-opaque-of-impl

What: revealing an opaque type in `Reflection`, `PostAnalysis` and `Codegen`
(`project_goals/opaque_types.rs`) first asks `Interner::opaque_of_impl` (`rustc_type_ir` patch
`0002`). It falls back to `type_of` of the opaque only when the hook says `None`, then normalizes
either.

Why: see that patch. hir-ty's trait-method opaque types, once their arguments name an impl,
reveal to the impl method's opaque type.

### 0002-unsafe-code-policy

What: `#![deny(unsafe_code)]` on the crate.

Why: `docs/v2/plan/design/unsafe.md`. The crate has no `unsafe` of its own. It is `deny`, not
`forbid`, because `TrivialTypeTraversalImpls!` (in `coherence`) expands to `rustc_type_ir`'s
allowed `unsafe impl GenericTypeVisitable`, and a `forbid` would refuse that `allow`.

### 0003-rustc-rules-at-unimplemented-sites

What: each `unimplemented!` upstream leaves on a path hir-ty can reach is replaced:
- `canonicalizer`, `InferConst::Fresh`: the same panic as the type arm; fresh variables are
  never canonicalized.
- `effect_goals`: `[const] FnPtr` and `[const] AsyncFn*` builtin candidates are `NoSolution`.
  No const impl of them exists, as upstream already answers for `[const] Iterator`.
- `solve::compute_const_arg_has_type_goal`:
  - a non-rigid `ConstKind::Alias` after `structurally_normalize_const` is `unreachable!`,
    because the solver never binds an inference variable to a non-rigid alias;
  - `ConstKind::Expr` is `unreachable!`, because the interner's `generic_const_exprs` is off
    and nothing lowers one.
- `normalizes_to`, `<unsafe<..> T as Pointee>::Metadata`: it is `T`'s metadata when `T` names
  no lifetime of the binder. Otherwise it is `()` given `for<..> T: Sized`, since an unsafe
  binder is as sized as the type it binds.
- `normalizes_to`, the discriminant of an unsafe binder: `discriminant_ty`, as for any other
  type without variants.

Why: blaze stops with an error, never a panic stub, on input rustc accepts
(`docs/v2/plan/goals/principles.md`). Each answer is the one rustc's own rules give, or an
`unreachable!` with the invariant that rules the case out.

## Dropped since the previous base (ra-ap-rustc_next_trait_solver 0.166.0, rustc `ce89c898`)

- The backport of rust-lang/rust#161380 (a constant sends a goal back to the caller's typing
  mode only when it names an opaque type or under `generic_const_exprs`): upstream has it.
- The backport of rust-lang/rust#159228 (an overflowing root goal is retried with twice the
  recursion limit, also for proof trees): upstream has it, and with it the
  `emit_next_solver_overflow_fcw` warning blaze's backport left out.
- The `[const] Iterator` hunk of the old `unimplemented!` patch: upstream answers `NoSolution`.
- The reservation-impl hunks of that patch (`NoSolution` outside coherence, ambiguous in it):
  upstream removed `ImplPolarity::Reservation`, so no impl has that polarity any more.
- `#![forbid(unsafe_code)]`: now `deny` (patch 0002), for the reason given there.
