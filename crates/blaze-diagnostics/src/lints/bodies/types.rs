//! rustc's late lint passes of values, numbers, pointers and casts in bodies.
//!
//! - `limits`: `TypeLimits` (`overflowing_literals`, `unused_comparisons`,
//!   `invalid_nan_comparisons`, `ambiguous_wide_pointer_comparisons`,
//!   `unpredictable_function_pointer_comparisons`).
//! - `values`: `InvalidValue`, `EnumIntrinsicsNonEnums`, `MutableTransmutes`, `CheckTransmutes`,
//!   `InteriorMutableConsts`.
//! - `pointers`: `DerefNullPtr`, `InvalidReferenceCasting`, `ImplicitAutorefs`, `PtrNullChecks`,
//!   `DanglingPointers`.
//! - `casts`: `StaticMutRefs`, `FunctionCastsAsInteger`, `ImplicitProvenanceCasts`,
//!   `RawBorrowsViaReferences`.
//! - `util`: what they share.
//!
//! Follows rustc's `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedLateLintModPass`) (MIT or
//! Apache-2.0).

mod casts;
mod limits;
mod pointers;
pub(crate) mod util;
mod values;

/// rustc's late lint passes of values, numbers, pointers and casts in bodies.
pub(super) fn passes() -> Vec<Box<dyn super::LatePass>> {
    let mut passes = limits::passes();
    passes.extend(values::passes());
    passes.extend(pointers::passes());
    passes.extend(casts::passes());
    passes
}
