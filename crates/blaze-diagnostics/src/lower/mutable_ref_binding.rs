//! E0658 "binding cannot be both mutable and by-reference": a `mut` binding where the default
//! binding mode is by reference, in edition 2024 under `ref_pat_eat_one_layer_2024` (or its
//! `_structural` form) without `mut_ref`; rustc's feature-gate error of `mut_ref`, on the
//! `mut ` before the name.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_pat_ident`) and
//! `compiler/rustc_session/src/parse.rs` (`feature_err`, `add_feature_diagnostics_for_issue`)
//! with `compiler/rustc_session/src/diagnostics.rs` (`FeatureGateError`,
//! `FeatureDiagnosticForIssue`, `FeatureDiagnosticHelp`, `SuggestUpgradeCompiler`) (MIT or
//! Apache-2.0); `mut_ref`'s issue is `compiler/rustc_feature/src/unstable.rs`'s.

use hir::InFile;
use syntax::{
    AstNode, TextRange,
    ast::{self, HasName as _},
};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

/// The tracking issue of `mut_ref`.
const MUT_REF_ISSUE: u32 = 123076;

/// The commit date of the pinned nightly (`harness/toolchain`, `nightly-2026-10-27`): rustc's
/// `CFG_VER_DATE`, which its upgrade note names. rustc's UI tests write `YYYY-MM-DD` for it
/// (`-Z ui-testing`).
pub(crate) const BUILT_ON: &str = "2026-10-26";

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MutableRefBinding) -> Vec<Found> {
    let node = cx.node(d.pat);
    let range = match node.value.as_ref().right() {
        Some(ast::Pat::IdentPat(pat)) => {
            let start = pat.syntax().text_range().start();
            let name = pat.name().map_or(start, |it| it.syntax().text_range().start());
            TextRange::new(start, name)
        }
        _ => node.value.syntax().text_range(),
    };
    let span = cx.span(InFile::new(node.file_id, range));
    let mut diag =
        Diag::error(Some("E0658"), "binding cannot be both mutable and by-reference", span);
    diag.note(format!(
        "see issue #{MUT_REF_ISSUE} <https://github.com/rust-lang/rust/issues/{MUT_REF_ISSUE}> for more information"
    ));
    diag.help("add `#![feature(mut_ref)]` to the crate attributes to enable");
    diag.note(format!(
        "this compiler was built on {BUILT_ON}; consider upgrading it if it is out of date"
    ));
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)]
}
