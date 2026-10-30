//! The stability checks: `deprecated` and `deprecated_in_future` of a use of a deprecated
//! definition (the crate's own or a dependency's: an item, a field, an associated item, a
//! variant, a macro), E0658 of a use of an unstable one of a crate with
//! `#![feature(staged_api)]`, `useless_deprecated` and the `unused_attributes` of a
//! `#[deprecated]` that does nothing, and the warning of `#[macro_escape]`.
//!
//! Each is reported where rustc reports it: a path name resolution resolves by the
//! `Checker` of `check_mod_unstable_api_usage` after `check_mod_attrs`, module by module
//! (`Phase::Items`); a method call, a field and a type-relative path in a body by typeck
//! (`Phase::Bodies`); a macro by resolution (`Phase::Resolve`); a `#[deprecated]` on a target
//! it can't be on by the attribute parser, whose lints are emitted after the checks of each
//! module (`emit_ast_lowering_delayed_lints`).
//!
//! Cost: one walk of the crate's syntax checks every use as it comes to it (`walk.rs`); a
//! use is a probe of the per-check index (`lookup.rs`) and stores nothing. Text (paths,
//! messages) is made here, only for what is reported, and only what is reported is sorted
//! into rustc's order.
//!
//! Follows `compiler/rustc_middle/src/middle/stability.rs` (`deprecation_message`,
//! `deprecation_suggestion`, `early_report_macro_deprecation`, `eval_stability_allow_unstable`,
//! `check_stability_allow_unstable`, `report_unstable`, `skip_stability_check_due_to_privacy`),
//! `compiler/rustc_passes/src/stability.rs` (`check_mod_unstable_api_usage`, `Checker`),
//! `compiler/rustc_resolve/src/macros.rs` (`check_stability_and_deprecation`),
//! `compiler/rustc_resolve/src/build_reduced_graph.rs` (`contains_macro_use`) and
//! `compiler/rustc_session/src/parse.rs` (`feature_err_issue`,
//! `add_feature_diagnostics_for_issue`) (MIT or Apache-2.0).

mod attr;
mod def;
mod lookup;
mod target;
mod walk;

use blaze_items::Fact;
use hir::{InFile, ModuleDef};
use syntax::{SyntaxNodePtr, TextRange};

use self::{
    attr::{Deprecation, Since, Stability},
    def::Def,
};
use crate::{
    cx::Cx,
    lints::syntax::source_fact,
    lower::{Found, Phase, mutable_ref_binding::BUILT_ON},
    model::{Applicability, Diag, Level, MultiSpan},
};

pub(super) fn lints(cx: &Cx<'_, '_>) -> Vec<Found> {
    walk::walk(cx)
}

/// Where a use is, as reported.
#[derive(Clone, Copy)]
struct Site {
    /// Where the lint's level is decided.
    at: InFile<SyntaxNodePtr>,
    span: InFile<TextRange>,
    phase: Phase,
}

/// `deprecated`/`deprecated_in_future` of a use of `def` (its kind and path printed by rustc's
/// `def_descr` and `def_path_str`), with the suggestion at `suggestion` when there is one.
fn deprecated(
    cx: &Cx<'_, '_>,
    def: Def,
    depr: &Deprecation,
    site: Site,
    suggestion: Option<InFile<TextRange>>,
) -> Option<Found> {
    let kind = def.descr(cx.db);
    let path = def::path(cx, def)?;
    let mut diag = deprecation_diag(cx, depr, kind, &path, site.span, suggestion);
    diag.fact_opt(definition_fact(cx, def), format!("the attributes of `{path}`"));
    source_fact(cx, &mut diag, site.at);
    Some(Found::lint(diag, site.phase, site.at))
}

/// `deprecated` of the parent segment `def` of a path to an item allowed through unstable
/// modules, with the item's message (`Checker::visit_path`).
fn deprecated_through(cx: &Cx<'_, '_>, def: Def, message: &str, site: Site) -> Option<Found> {
    let depr = Deprecation { since: Since::Unspecified, note: Some(message.to_owned()), suggestion: None };
    deprecated(cx, def, &depr, site, None)
}

/// The macro deprecation of `early_report_macro_deprecation`: kind `macro`, the path as written.
fn deprecated_macro(cx: &Cx<'_, '_>, mac: hir::Macro, depr: &Deprecation, path: &str, site: Site) -> Found {
    let mut diag = deprecation_diag(cx, depr, "macro", path, site.span, Some(site.span));
    diag.fact_opt(definition_fact(cx, Def::Item(ModuleDef::Macro(mac))), format!("the attributes of `{path}`"));
    source_fact(cx, &mut diag, site.at);
    Found::lint(diag, site.phase, site.at)
}

/// `contains_macro_use`'s warning of a `#[macro_escape]` at `range`, on the `mod` at `at`.
fn macro_escape(cx: &Cx<'_, '_>, range: InFile<TextRange>, inner: bool, at: InFile<SyntaxNodePtr>) -> Found {
    let mut diag = Diag::new(Level::Warning, None, "`#[macro_escape]` is a deprecated synonym for `#[macro_use]`");
    diag.span = MultiSpan::of(cx.span(range));
    if inner {
        diag.help("try an outer attribute: `#[macro_use]`");
    }
    source_fact(cx, &mut diag, at);
    Found::new(diag, Phase::Resolve)
}

/// `report_unstable` of a use of `def` (whose path, for the fact, is `path`).
fn unstable(cx: &Cx<'_, '_>, def: Def, path: Option<&str>, stability: &Stability, site: Site) -> Option<Found> {
    let mut diag = unstable_diag(cx, stability, site.span)?;
    let path = path.map(str::to_owned).or_else(|| def::path(cx, def));
    if let Some(path) = path {
        diag.fact_opt(definition_fact(cx, def), format!("the stability of `{path}`"));
    }
    source_fact(cx, &mut diag, site.at);
    Some(Found::new(diag, site.phase))
}

fn definition_fact(cx: &Cx<'_, '_>, def: Def) -> Option<Fact> {
    match def.item(cx.db) {
        ModuleDef::Module(module) => Some(Fact::Signature(cx.module_key(module))),
        _ => def.item_id(cx.db).map(|it| Fact::Signature(cx.key(it))),
    }
}

/// `stability::Deprecated` of a use of the `kind` `path` deprecated by `depr`, with its
/// suggestion at `suggestion` when there is one.
fn deprecation_diag(
    cx: &Cx<'_, '_>,
    depr: &Deprecation,
    kind: &str,
    path: &str,
    span: InFile<TextRange>,
    suggestion: Option<InFile<TextRange>>,
) -> Diag {
    let note = depr.note.as_ref().map(|it| format!(": {it}")).unwrap_or_default();
    let (lint, message) = match (&depr.since, depr.is_in_effect()) {
        (_, true) => ("deprecated", format!("use of deprecated {kind} `{path}`{note}")),
        (Since::RustcVersion(version), false) => (
            "deprecated_in_future",
            format!("use of {kind} `{path}` that will be deprecated in future version {version}{note}"),
        ),
        (_, false) => (
            "deprecated_in_future",
            format!("use of {kind} `{path}` that will be deprecated in a future Rust version{note}"),
        ),
    };
    let mut diag = Diag::lint(lint, message, cx.span(span));
    if let Some(replacement) = &depr.suggestion
        && let Some(at) = suggestion
    {
        diag.suggestion_verbose(
            cx.span(at),
            format!("replace the use of the deprecated {kind}"),
            replacement.clone(),
            Applicability::MachineApplicable,
        );
    }
    diag
}

/// `report_unstable` through `feature_err_issue`: E0658 of a use of an unstable library
/// feature, on a nightly compiler.
fn unstable_diag(cx: &Cx<'_, '_>, stability: &Stability, span: InFile<TextRange>) -> Option<Diag> {
    let Stability::Unstable { feature, reason, issue, .. } = stability else { return None };
    let feature = feature.as_str();
    let message = match reason {
        Some(reason) => format!("use of unstable library feature `{feature}`: {reason}"),
        None => format!("use of unstable library feature `{feature}`"),
    };
    let mut diag = Diag::error(Some("E0658"), message, cx.span(span));
    if let Some(issue) = issue {
        diag.note(format!("see issue #{issue} <https://github.com/rust-lang/rust/issues/{issue}> for more information"));
    }
    diag.help(format!("add `#![feature({feature})]` to the crate attributes to enable"));
    diag.note(format!("this compiler was built on {BUILT_ON}; consider upgrading it if it is out of date"));
    Some(diag)
}
