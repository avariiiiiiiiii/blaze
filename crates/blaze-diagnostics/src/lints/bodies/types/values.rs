//! rustc's late lint passes of the values made, transmuted and named in bodies.
//!
//! - `invalid`: `InvalidValue` (`invalid_value`): `mem::zeroed`, `mem::uninitialized`, a
//!   transmute of zero and `MaybeUninit::{zeroed, uninit}().assume_init()` of a type that does
//!   not permit it, with `inhabited`, rustc's `InhabitedPredicate`.
//! - `intrinsics`: `EnumIntrinsicsNonEnums` (`enum_intrinsics_non_enums`).
//! - `transmutes`: `MutableTransmutes` (`mutable_transmutes`) and `CheckTransmutes`
//!   (`ptr_to_integer_transmute_in_consts`, `unnecessary_transmutes`,
//!   `integer_to_ptr_transmutes`).
//! - `interior`: `InteriorMutableConsts` (`const_item_interior_mutations`).
//!
//! Follows rustc's `compiler/rustc_lint/src/builtin.rs` (`MutableTransmutes`, `InvalidValue`),
//! `compiler/rustc_lint/src/enum_intrinsics_non_enums.rs`, `compiler/rustc_lint/src/transmute.rs`,
//! `compiler/rustc_lint/src/interior_mutable_consts.rs`, `compiler/rustc_lint/src/lints.rs` (the
//! messages) and `compiler/rustc_middle/src/ty/inhabitedness/` (MIT or Apache-2.0).

mod inhabited;
mod interior;
mod intrinsics;
mod invalid;
mod transmutes;

use hir::db::HirDatabase;
use hir_def::{FunctionId, HasModule as _, hir::ExprId};
use hir_ty::{
    intrinsic::{Intrinsic, intrinsic_of},
    next_solver::{DbInterner, Region, Ty},
};
use syntax::AstNode as _;

use crate::lints::bodies::{BodyCx, LatePass, types::util};

/// rustc's late lint passes of the values made, transmuted and named in bodies.
pub(super) fn passes() -> Vec<Box<dyn LatePass>> {
    vec![
        Box::new(invalid::InvalidValue),
        Box::new(intrinsics::EnumIntrinsicsNonEnums),
        Box::new(transmutes::MutableTransmutes),
        Box::new(transmutes::CheckTransmutes::default()),
        Box::new(interior::InteriorMutableConsts),
    ]
}

/// rustc's `is_intrinsic(def, sym::transmute)`: `def` is the intrinsic `transmute`. An
/// intrinsic rustc doesn't have is none of rustc's, `transmute` included; MIR lowering refuses
/// its calls.
fn is_transmute(db: &dyn HirDatabase, def: FunctionId) -> bool {
    matches!(intrinsic_of(db, def), Ok(Some(Intrinsic::Transmute)))
}

/// `ty` with its free regions erased, as rustc's type checking results have them.
fn erased<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Ty<'db> {
    rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_erased(interner))
}

/// rustc's printing of a type of the body with its regions erased, as it prints the types of
/// its type checking results.
fn print<'db>(b: &BodyCx<'_, 'db>, near: ExprId, ty: Ty<'db>) -> Option<String> {
    util::print_ty(b, near, erased(DbInterner::new_with(b.db(), b.owner.krate(b.db())), ty))
}

/// rustc's printing of `ty` under `with_no_trimmed_paths!`: every path in full
/// (`std::ptr::NonNull<i32>`).
fn print_untrimmed<'db>(b: &BodyCx<'_, 'db>, near: ExprId, ty: Ty<'db>) -> Option<String> {
    let node = util::syntax(b, near)?;
    let expr = syntax::ast::Expr::cast(node.value)?;
    let of = b.cx.sema.type_of_expr(&expr)?.original;
    Some(crate::print::ty_untrimmed(b.cx, &of.derived_from_next_solver(ty)))
}
