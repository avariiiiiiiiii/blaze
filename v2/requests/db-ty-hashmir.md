# Requests from db-ty-hashmir

`MirLowerError<'db>` now derives `Hash` (so MIR query outputs get a value fingerprint and keep
their early cutoff after `evict_values`). It holds `LayoutError`, and `MirEvalError` (reached
through `ConstEvalError`) holds it too, so these must implement `Hash` or `crates/hir-ty` does not
compile.

1. **What:** add `Hash` to the derive of `LayoutError`.
   **Where:** `crates/hir-ty/src/layout.rs` (`#[derive(Debug, PartialEq, Eq, Clone)] pub enum
   LayoutError`, items lane). It becomes `#[derive(Debug, PartialEq, Eq, Hash, Clone)]`.
   **Why:** `MirLowerError::LayoutError(LayoutError)` and `MirEvalError::LayoutError(LayoutError,
   StoredTy)` in `crates/hir-ty/src/mir.rs` derive `Hash`. `PartialEq` is derived, so a derived
   `Hash` agrees with it.

2. **What:** add `Hash` to the derive of `LayoutCalculatorError<F>`.
   **Where:** `crates/rustc_abi/src/layout.rs` (`#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub
   enum LayoutCalculatorError<F>`). It becomes `#[derive(Copy, Clone, Debug, PartialEq, Eq,
   Hash)]`.
   **Why:** `LayoutError::BadCalc(LayoutCalculatorError<()>)` has to hash for item 1. Every
   variant holds only `F` or `usize`, so the derive compiles for `F = ()`, and it agrees with the
   derived `PartialEq`.

(`ConstEvalError<'db>` in `crates/hir-ty/src/consteval.rs` also needs `Hash`. That file belongs
to `db-ty`, so it is in the report rather than here.)
