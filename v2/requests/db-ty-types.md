# Requests of db-ty-types

## 1. db-ty-hir: drop `collect_ty_garbage` from hir's re-export

What: in `crates/hir/src/lib.rs` line 220, replace

```rust
        next_solver::{clear_tls_solver_cache, collect_ty_garbage},
```

with

```rust
        next_solver::clear_tls_solver_cache,
```

Why: interned types are never freed now (the `intern` crate has no GC or reference counts, and
`next_solver` interns into its own `u32`-indexed tables), so `collect_ty_garbage` and the GC
visitor impls are gone from `crates/hir-ty/src/next_solver/interner.rs`. Nothing called it
(`rg collect_ty_garbage`); this is the same change as `v2/requests/format.md` request 1. Until
it is applied, `hir` does not compile.

## 2. Lane db: interner-free constructors in `vendor/rustc_type_ir`

What: a constructor that takes no interner for each struct whose private `()` field forces one
through `new_from_args(interner, ..)`, so a decoder can rebuild them from their fields:

```diff
--- a/vendor/rustc_type_ir/src/predicate.rs
+++ b/vendor/rustc_type_ir/src/predicate.rs
@@ impl<I: Interner> TraitRef<I> {
     pub fn new_from_args(interner: I, trait_def_id: I::TraitId, args: I::GenericArgs) -> Self {
         interner.debug_assert_args_compatible(trait_def_id.into(), args);
         Self { def_id: trait_def_id, args, _use_trait_ref_new_instead: () }
     }
+
+    /// A trait reference of fields that were checked when it was first made, e.g. read back
+    /// from an interned row.
+    pub fn from_checked_parts(def_id: I::TraitId, args: I::GenericArgs) -> Self {
+        Self { def_id, args, _use_trait_ref_new_instead: () }
+    }
@@ impl<I: Interner> ExistentialTraitRef<I> {
     pub fn new_from_args(interner: I, trait_def_id: I::TraitId, args: I::GenericArgs) -> Self {
         interner.debug_assert_existential_args_compatible(trait_def_id.into(), args);
         Self { def_id: trait_def_id, args, _use_existential_trait_ref_new_instead: () }
     }
+
+    /// As [`TraitRef::from_checked_parts`].
+    pub fn from_checked_parts(def_id: I::TraitId, args: I::GenericArgs) -> Self {
+        Self { def_id, args, _use_existential_trait_ref_new_instead: () }
+    }
@@ impl<I: Interner> ExistentialProjection<I> {
     ) -> ExistentialProjection<I> {
         interner.debug_assert_existential_args_compatible(def_id.into(), args);
         Self { def_id, args, term, use_existential_projection_new_instead: () }
     }
+
+    /// As [`TraitRef::from_checked_parts`].
+    pub fn from_checked_parts(
+        def_id: I::TraitAssocTermId,
+        args: I::GenericArgs,
+        term: I::Term,
+    ) -> Self {
+        Self { def_id, args, term, use_existential_projection_new_instead: () }
+    }
@@ impl<I: Interner> AliasTerm<I> {
     ) -> AliasTerm<I> {
         interner.debug_assert_args_compatible(kind.def_id(), args);
         AliasTerm { kind, args, _use_alias_term_new_instead: () }
     }
+
+    /// As [`TraitRef::from_checked_parts`].
+    pub fn from_checked_parts(kind: AliasTermKind<I>, args: I::GenericArgs) -> Self {
+        AliasTerm { kind, args, _use_alias_term_new_instead: () }
+    }
--- a/vendor/rustc_type_ir/src/ty_kind.rs
+++ b/vendor/rustc_type_ir/src/ty_kind.rs
@@ impl<I: Interner> AliasTy<I> {
     pub fn new_from_args(interner: I, kind: AliasTyKind<I>, args: I::GenericArgs) -> AliasTy<I> {
         interner.debug_assert_args_compatible(kind.def_id(), args);
         AliasTy { kind, args, _use_alias_ty_new_instead: () }
     }
+
+    /// As [`crate::TraitRef::from_checked_parts`].
+    pub fn from_checked_parts(kind: AliasTyKind<I>, args: I::GenericArgs) -> AliasTy<I> {
+        AliasTy { kind, args, _use_alias_ty_new_instead: () }
+    }
```

Why: `crates/hir-ty/src/next_solver/persist.rs` decodes these from interned rows and persisted
type tables, where no `DbInterner` exists (`kind()` takes none). Today it writes them field by
field through `MaybeUninit` (its `assemble!` macro), which is sound only because the private
field is a `()` and `DbInterner`'s two `debug_assert_*_args_compatible` are empty. With these,
`assemble!` goes and each use becomes `X::from_checked_parts(..)`; I make that change when this
lands.

## 3. Lane db (optional): flags of `GenericArgs` tested in blocks

What: none needed today. `Clauses` keeps its flags and outer binder in columns of its list table
(`Lists::intern_with`), as `Clauses: Flags` requires. `GenericArgs` is not `Flags` in today's
`Interner` trait, so `has_type_flags` over arguments visits each one (each a column read). If
lane db wants arguments tested in one read, the change is `type GenericArgs: .. + Flags` in
`vendor/rustc_type_ir/src/interner.rs` and a `TypeVisitableExt` fast path; my side would then
intern `GenericArgs` with `intern_with`, as `Clauses` is.

## Applied requests of others

- db-ty-solver request 1 (drop `reinit_cache`): done in `interner.rs` (`new_with`, `attach`,
  `attach_allow_change` and its `Drop`; the comments that named it).
- db-ty-db request 3 (`allocation.rs`): `impl Hash for AllocationData` hashes `memory_map`;
  `Allocation::new(AllocationData { ty, memory, memory_map })` and its `Deref` are kept;
  `TyTableWriter::ty`/`TyTableReader::ty` are in `next_solver/persist.rs` with shared rule 2's
  signatures.
