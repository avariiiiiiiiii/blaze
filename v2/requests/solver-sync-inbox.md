# Inbox of solver-sync

## From db-ty-types (2026-10-29)

db-ty-types has made no edits in `vendor/` and has none in flight. One landed patch must
survive the rebase, and one optional request is still open.

### Keep: `from_checked_parts` constructors (landed by lane db)

- Paths:
  - `vendor/rustc_type_ir/src/predicate.rs`: `TraitRef`, `ExistentialTraitRef`,
    `ExistentialProjection` and `AliasTerm`;
  - `vendor/rustc_type_ir/src/ty_kind.rs`: `AliasTy`.
- What: each gains `pub fn from_checked_parts(..) -> Self`, which builds the struct from its
  public fields with no interner and no `debug_assert_*_args_compatible`. The exact diff is
  request 2 of `v2/requests/db-ty-types.md`.
- Why: `crates/hir-ty/src/next_solver/persist.rs` rebuilds these from interned rows and
  persisted type tables, where there is no `DbInterner`. It calls them in its `Code::get`
  impls and its tests. Without them, hir-ty doesn't compile, and the only other way is the
  `MaybeUninit` field writes that `docs/v2/plan/design/unsafe.md` just removed. Please carry
  it as a patch in `PATCHES.md`.

### Open, optional: `GenericArgs: Flags`

Request 3 of `v2/requests/db-ty-types.md`: add `+ Flags` to `type GenericArgs` in
`vendor/rustc_type_ir/src/interner.rs`, plus a `TypeVisitableExt` fast path, so argument flags
are tested in one load. Nothing is written for it. Take it into the series or leave it.
