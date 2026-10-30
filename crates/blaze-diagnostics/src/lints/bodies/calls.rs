//! rustc's late lint passes of calls, statements, `let`s and patterns in bodies.
//!
//! - `statements`: `PathStatements`, `LetUnderscore`, `UnusedResults`, `UnitBindings`,
//!   `NonShorthandFieldPatterns`, `UnusedAllocation`.
//! - `items`: `DropForgetUseless`, `InvalidFromUtf8`, `InvalidAtomicOrdering`.
//! - `desugar`: `ForLoopsOverFallibles`, `ShadowedIntoIter`, `IfLetRescope`.
//! - `methods`: `NoopMethodCall`, `MapUnitFn`, `NonPanicFmt`, `AsmLabels`.
//!
//! Follows rustc's `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedLateLintModPass`) (MIT or
//! Apache-2.0).

mod desugar;
mod items;
mod methods;
mod statements;

/// rustc's late lint passes of calls, statements, `let`s and patterns in bodies.
pub(super) fn passes() -> Vec<Box<dyn super::LatePass>> {
    let mut passes = statements::passes();
    passes.extend(items::passes());
    passes.extend(desugar::passes());
    passes.extend(methods::passes());
    passes
}
