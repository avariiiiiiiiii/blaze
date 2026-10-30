//! rustc's late lint passes of method calls, panics and `asm!` in bodies.
//!
//! - `noop_method_call`: `NoopMethodCall` (`noop_method_call`, `suspicious_double_ref_op`).
//! - `map_unit_fn`: `MapUnitFn` (`map_unit_fn`).
//! - `non_fmt_panic`: `NonPanicFmt` (`non_fmt_panics`).
//! - `asm_labels`: `AsmLabels` (`named_asm_labels`, `binary_asm_labels`).
//!
//! Follows rustc's `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedLateLintModPass`) (MIT or
//! Apache-2.0).

mod asm_labels;
mod map_unit_fn;
mod non_fmt_panic;
mod noop_method_call;

use crate::lints::bodies::LatePass;

/// rustc's late lint passes of method calls, panics and `asm!` in bodies.
pub(super) fn passes() -> Vec<Box<dyn LatePass>> {
    vec![non_fmt_panic::pass(), noop_method_call::pass(), asm_labels::pass(), map_unit_fn::pass()]
}
