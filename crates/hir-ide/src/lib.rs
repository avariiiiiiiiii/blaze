//! The final bits of analysis; Display, extra diagnostics, and tests.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
// It's useful to refer to code that is private in doc comments.
#![allow(rustdoc::private_intra_doc_links)]

extern crate ra_ap_rustc_ast_ir as rustc_ast_ir;
extern crate rustc_abi;
extern crate rustc_type_ir;

pub mod const_macro;
pub mod diagnostics;
pub mod display;
pub mod mir_pretty;

#[cfg(test)]
mod test_db;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use base_db::Db;
use hir_def::ModuleId;
use hir_ty::next_solver::Const;
use syntax::ast::{ConstArg, make};

pub use hir_ty::*;
/// hir-ty's `mir` with the passes over it.
pub use hir_ty_mir::mir;

use crate::display::HirDisplay;

pub fn known_const_to_ast<'db>(
    konst: Const<'db>,
    db: &'db Db,
    target_module: ModuleId,
) -> Option<ConstArg> {
    Some(make::expr_const_value(
        &konst.display_source_code(db, target_module, true).unwrap_or_else(|_| "_".to_owned()),
    ))
}
