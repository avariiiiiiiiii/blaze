//! MIR's passes over hir-ty's MIR: lowering a body to it, the passes to runtime MIR and
//! rustc's optimizations, monomorphization, the interpreter, and printing a body.
//!
//! hir-ty has MIR's types and errors (`hir_ty::mir`) and what it asks of these passes, a
//! body, a constant's value, is a method of `HirDatabase` that hir-ide's
//! `impl_hir_database!` implements with them. An edit of a pass compiles this crate again,
//! not hir-ty. The passes see hir-ty's modules at this crate's root, and its `mir` as their
//! parent, as when they were in hir-ty.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![allow(rustdoc::private_intra_doc_links)]

extern crate ra_ap_rustc_ast_ir as rustc_ast_ir;
extern crate ra_ap_rustc_index as rustc_index;

use hir_def::CallableDefId;
#[allow(unused_imports, reason = "hir-ty's root, which the passes name as `crate::`")]
use hir_ty::*;

pub mod mir;
