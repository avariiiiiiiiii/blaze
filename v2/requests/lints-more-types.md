# Requests of lints-more-types (crates/blaze-lints, child of lane `lints`)

## db (`crates/hir`): five type questions clippy's `needless_pass_by_value` and `multiple_unsafe_ops_per_block` ask

- What, in `crates/hir/src/lib.rs`:
  - `impl<'db> Type<'db> { pub fn env_bounds(&self, db: &'db Db) -> Vec<(Trait, Vec<Type<'db>>)> }`: the trait
    clauses of the type's param env, elaborated (`traits::elaborate`), not global, other than `Sized` and
    `MetaSized`, whose self type is exactly `self` (no autoderef, unlike `env_traits`), each with its generic
    arguments after `Self`. clippy: `traits::elaborate(cx.param_env.caller_bounds())...filter(|t| t.self_ty() == ty)`.
  - `pub fn ref_to(&self, db: &'db Db, mutability: Mutability) -> Type<'db>`: `&'erased self` in the same env
    (clippy's `Ty::new_imm_ref(tcx, re_erased, ty)`), so `impls_trait` can be asked of it.
  - `pub fn is_sized(&self, db: &'db Db) -> bool`: `ty.is_sized(tcx, typing_env)` in the type's env.
  - `pub fn may_implement_copy(&self, db: &'db Db) -> bool`: rustc's
    `type_allowed_to_implement_copy(..).is_ok()`: an ADT with no `Drop` impl whose every field, with the
    type's arguments, is `Copy` in the env.
  - `impl Callable<'_> { pub fn safety(&self) -> hir_ty::next_solver::abi::Safety }`: the signature's
    safety, for an `unsafe fn` pointer callee (`ty::FnPtr(sig_tys, hdr) => sig.safety()`).
- Why: blaze-lints ports clippy's `needless_pass_by_value` (the workspace denies it) and
  `multiple_unsafe_ops_per_block`; they ask exactly these of rustc's trait system, and hir has
  no public way to ask them. Semantics belong in the front end (semantics-in-front-end), so the
  lints ask hir rather than rebuilding the solver's answers.

## lints-driver (`crates/blaze-lints/src/check.rs`, `lints/mod.rs`)

Told by tmux, with the whole list in `/tmp/blaze-lanes/lints-more-types-to-driver.md`: the 12
lints in `implemented!`, `On` widened to `u64`, `mod more_types_tests;`, the calls from the
walks, and six `Cx` additions (`path` at item level, `resolve_type`, `fn_def`/`adt_def`,
`is_exported`, `is_in_test`, `def_span`).
