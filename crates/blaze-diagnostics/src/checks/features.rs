//! The errors of the crate's `#![feature(..)]` and of what it uses without the feature: rustc's
//! `features` as expansion reads the crate's attributes (E0557 of a removed language feature,
//! E0725 of one `-Z allow-features` leaves out), `check_crate`'s checks of the features
//! themselves (E0554 off the nightly channel, incompatible and dependent features), its feature
//! gates (E0658, [`gates`]), and `check_unused_or_stable_features` ([`library`]: E0635, E0557 of a
//! removed library feature, `stable_features`, `duplicate_features`). The gates of attributes,
//! which rustc's attribute parser checks, are [`attributes`]'s, for attribute parsing to give.
//!
//! - [`Features`]: what the crate enables (rustc's `Features`), with [`feature_err`] and
//!   [`feature_warn`] (the E0658 error of a gate and the warning of a soft one) and
//!   [`allows_unstable`] (what a macro's `#[allow_internal_unstable]` lets its expansion use).
//!
//! `-Z allow-features`, `-Z next-solver=globally` and `-Z internal-testing-features` are not
//! among the options the driver passes ([`Flags`]); `RUSTC_BOOTSTRAP` is read from the crate's
//! environment.
//!
//! Follows rustc's `compiler/rustc_expand/src/config.rs` (`features`), `compiler/rustc_expand/
//! src/diagnostics.rs` (`FeatureRemoved`, `FeatureRemovedReason`, `FeatureNotAllowed`),
//! `compiler/rustc_ast_passes/src/feature_gate.rs` (`check_crate`, `maybe_stage_features`,
//! `check_incompatible_features`, `check_dependent_features`,
//! `check_new_solver_banned_features`, `check_features_requiring_new_solver`),
//! `compiler/rustc_ast_passes/src/diagnostics.rs` (`FeatureOnNonNightly`, `StableFeature`,
//! `IncompatibleFeatures`, `MissingDependentFeatures`), `compiler/rustc_feature/src/lib.rs`
//! (`UnstableFeatures::from_environment`, `find_lang_feature_issue`),
//! `compiler/rustc_feature/src/unstable.rs` (`Features`, `INCOMPATIBLE_FEATURES`,
//! `DEPENDENT_FEATURES`), `compiler/rustc_session/src/diagnostics.rs` (`feature_err_issue`,
//! `feature_warn_issue`, `add_feature_diagnostics_for_issue`, `FeatureGateError`,
//! `FeatureDiagnosticForIssue`, `FeatureDiagnosticHelp`, `SuggestUpgradeCompiler`) and
//! `compiler/rustc_span/src/lib.rs` (`Span::allows_unstable`) (MIT or Apache-2.0).

pub(crate) mod attributes;
mod gates;
mod library;

use std::ops::ControlFlow;

use blaze_items::Fact;
use hir::{HirFileId, InFile};
use rustc_hash::FxHashSet;
use syntax::{AstNode, NodeOrToken, SyntaxKind, SyntaxNode, TextRange, ast};

use crate::{
    cx::Cx,
    lints::syntax::{
        Walk,
        attrs::{
            Owner, Target,
            features::{Named, named, table},
        },
    },
    lower::{Found, Phase, mutable_ref_binding::BUILT_ON},
    model::{Applicability, Code, Diag, Level, MultiSpan, Span},
};

/// What rustc says of the crate's features and of what it uses without them, each part called
/// by `checks::check` at its pass: [`configured`] before expansion, [`expansion`] as expansion
/// meets macro calls, [`early`] in `early_lint_checks`, [`unused_or_stable`] in
/// `misc_checking_1`.
///
/// The gates of the attributes of macro calls, as expansion gives them.
pub(crate) fn expansion(cx: &Cx<'_, '_>, walk: &Walk<'_>, owners: &[Owner], features: &Features) -> Vec<Found> {
    attributes::expansion(cx, walk, owners, features)
}

/// `feature_gate::check_crate`: the checks of the features themselves ([`staged`]), then the
/// gates of syntax.
pub(crate) fn early(cx: &Cx<'_, '_>, walk: &Walk<'_>, owners: &[Owner], features: &Features, flags: &Flags) -> Vec<Found> {
    let mut found = staged(cx, owners, features, flags);
    found.extend(gates::check(cx, walk, features, flags));
    found
}

/// `check_unused_or_stable_features`: E0635, E0557 of a removed library feature,
/// `stable_features`, `duplicate_features`.
pub(crate) fn unused_or_stable(cx: &Cx<'_, '_>, owners: &[Owner], features: &Features) -> Vec<Found> {
    library::check(cx, owners, features)
}

/// The `-Z` options rustc reads here, as rustc takes them when none is given.
#[derive(Clone, Debug, Default)]
pub(crate) struct Flags {
    /// `-Z allow-features=a,b`: the only features `#![feature]` may enable.
    pub(crate) allow_features: Option<Vec<String>>,
    /// `-Z next-solver=globally`.
    pub(crate) next_solver_globally: bool,
    /// `-Z internal-testing-features`.
    pub(crate) internal_testing_features: bool,
}

/// A feature `#![feature(..)]` enables: rustc's `EnabledLangFeature` and `EnabledLibFeature`.
#[derive(Clone, Debug)]
pub(crate) struct Enabled {
    pub(crate) name: String,
    /// Where the name is written.
    pub(crate) at: InFile<TextRange>,
    /// For a language feature rustc accepted, the version it did in, as its list writes it.
    pub(crate) since: Option<&'static str>,
}

/// The features the crate enables: rustc's `Features`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Features {
    /// The language features, in the order written.
    pub(crate) lang: Vec<Enabled>,
    /// The rest, which rustc takes for library features, in the order written.
    pub(crate) lib: Vec<Enabled>,
    names: FxHashSet<String>,
    /// rustc's `sess.unstable_features.is_nightly_build()`: what its feature errors suggest.
    pub(crate) nightly: bool,
    /// rustc's `sess.opts.unstable_features.is_nightly_build()`: whether `#![feature]` may be
    /// used (`RUSTC_BOOTSTRAP` may name the crate).
    pub(crate) crate_nightly: bool,
}

impl Features {
    /// rustc's `Features::enabled`: whether `#![feature]` enables `name`, of the language or
    /// not.
    pub(crate) fn enabled(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// rustc's `enabled_features_iter_stable_order`: the language features, then the rest.
    pub(crate) fn in_stable_order(&self) -> impl Iterator<Item = &Enabled> + Clone {
        self.lang.iter().chain(&self.lib)
    }

    fn enable_lang(&mut self, name: &str, at: InFile<TextRange>, since: Option<&'static str>) {
        self.names.insert(name.to_owned());
        self.lang.push(Enabled { name: name.to_owned(), at, since });
    }

    fn enable_lib(&mut self, name: &str, at: InFile<TextRange>) {
        self.names.insert(name.to_owned());
        self.lib.push(Enabled { name: name.to_owned(), at, since: None });
    }
}

/// The features the crate enables, as attribute parsing and the gates read them.
pub(crate) fn enabled(cx: &Cx<'_, '_>, owners: &[Owner]) -> Features {
    configured(cx, owners, &Flags::default()).0
}

/// The crate's `#![feature(..)]`s: each feature it names, with the attribute it is in.
fn crate_features<'a>(owners: &'a [Owner]) -> Vec<Named<'a>> {
    owners.iter().find(|it| it.target == Target::Crate).map(|it| named(it).collect()).unwrap_or_default()
}

/// rustc's `features`: what `#![feature]` enables, and its errors as expansion reads the
/// crate's attributes: a removed language feature (E0557), one `-Z allow-features` leaves out
/// (E0725).
pub(crate) fn configured(cx: &Cx<'_, '_>, owners: &[Owner], flags: &Flags) -> (Features, Vec<Found>) {
    let bootstrap = cx.krate.base().env(cx.db).get("RUSTC_BOOTSTRAP");
    let crate_name = cx.krate.display_name(cx.db).map(|it| it.crate_name().to_string());
    let mut features = Features {
        nightly: nightly_build(bootstrap.as_deref(), None),
        crate_nightly: nightly_build(bootstrap.as_deref(), crate_name.as_deref()),
        ..Features::default()
    };
    let mut found = Vec::new();
    for feature in crate_features(owners) {
        let name = feature.name.as_str();
        let at = feature.attr.in_file(feature.range);
        let lang = table::lang(name);
        if let Some((reason, pull)) = table::removed(name) {
            let span = cx.span(at);
            let mut diag = Diag::error(Some("E0557"), "feature has been removed", span.clone());
            diag.span_label(span, "feature has been removed");
            let since = lang.map_or("", |it| it.since);
            let pull_note = pull
                .map(|pull| format!("; see <https://github.com/rust-lang/rust/pull/{pull}> for more information"))
                .unwrap_or_default();
            diag.note(format!("removed in {since}{pull_note}"));
            if let Some(reason) = reason {
                diag.note(reason);
            }
            crate_fact(cx, &mut diag);
            found.push(Found::new(diag, Phase::Expand));
            continue;
        }
        if let Some(lang) = lang.filter(|it| it.kind == table::Kind::Accepted) {
            features.enable_lang(name, at, Some(lang.since));
            continue;
        }
        if let Some(allowed) = &flags.allow_features
            && allowed.iter().all(|it| it != name)
        {
            let message = format!("the feature `{name}` is not in the list of allowed features");
            let mut diag = Diag::error(Some("E0725"), message, cx.span(at));
            crate_fact(cx, &mut diag);
            found.push(Found::new(diag, Phase::Expand));
            continue;
        }
        match lang {
            Some(lang) if lang.kind.is_unstable() => features.enable_lang(name, at, None),
            _ => features.enable_lib(name, at),
        }
    }
    (features, found)
}

/// rustc's `UnstableFeatures::from_environment(krate).is_nightly_build()` of blaze's nightly:
/// `RUSTC_BOOTSTRAP=-1` makes it a stable compiler, unless the same variable allows the crate
/// (`1`, or a list naming it); any other value leaves it nightly.
fn nightly_build(bootstrap: Option<&str>, krate: Option<&str>) -> bool {
    let Some(value) = bootstrap else { return true };
    let names_crate = krate.is_some_and(|name| value.split(',').any(|it| it == name));
    names_crate || value != "-1"
}

/// The `#[feature]` attributes and features rustc's `INCOMPATIBLE_FEATURES` lists: no two may be
/// enabled together.
const INCOMPATIBLE_FEATURES: &[(&str, &str)] =
    &[("ref_pat_eat_one_layer_2024", "ref_pat_eat_one_layer_2024_structural")];

/// rustc's `DEPENDENT_FEATURES`: each feature and those it needs enabled with it.
const DEPENDENT_FEATURES: &[(&str, &[&str])] = &[
    ("generic_const_args", &["min_generic_const_args"]),
    ("macroless_generic_const_args", &["min_generic_const_args"]),
    ("unsized_const_params", &["adt_const_params"]),
];

/// The checks `check_crate` makes of the features themselves before its gates:
/// `maybe_stage_features` (E0554), `check_incompatible_features`, `check_dependent_features`,
/// `check_new_solver_banned_features`, `check_features_requiring_new_solver`.
pub(crate) fn staged(cx: &Cx<'_, '_>, owners: &[Owner], features: &Features, flags: &Flags) -> Vec<Found> {
    let mut diags = Vec::new();
    if !features.crate_nightly
        && features.in_stable_order().next().is_some()
        && let Some(diag) = on_non_nightly(cx, owners, features)
    {
        diags.push(diag);
    }
    for (f1, f2) in INCOMPATIBLE_FEATURES {
        let first = |name: &str| features.in_stable_order().find(|it| it.name == name);
        if let (Some(a), Some(b)) = (first(f1), first(f2)) {
            let message = format!("`{f1}` and `{f2}` are incompatible, using them at the same time is not allowed");
            let mut diag = Diag::error(None, message, cx.span(a.at));
            diag.primary(cx.span(b.at));
            diag.help("remove one of these features");
            diags.push(diag);
        }
    }
    for (parent, children) in DEPENDENT_FEATURES {
        if !features.enabled(parent) || children.iter().all(|it| features.enabled(it)) {
            continue;
        }
        let Some(at) = features.in_stable_order().find(|it| it.name == *parent) else { continue };
        let missing: Vec<String> =
            children.iter().filter(|it| !features.enabled(it)).map(|it| format!("`{it}`")).collect();
        let message = format!("`{parent}` requires {} to be enabled", missing.join(", "));
        let mut diag = Diag::error(None, message, cx.span(at.at));
        diag.help("enable all of these features");
        diags.push(diag);
    }
    if flags.next_solver_globally
        && let Some(gce) = features.lang.iter().find(|it| it.name == "generic_const_exprs")
    {
        let message = "`-Znext-solver=globally` and `generic_const_exprs` are incompatible, using them at the same time is not allowed";
        let mut diag = Diag::error(None, message, cx.span(gce.at));
        diag.help("remove one of these features");
        diags.push(diag);
    }
    if !flags.next_solver_globally
        && let Some(gca) = features.lang.iter().find(|it| it.name == "generic_const_args")
    {
        let message = "`generic_const_args` requires -Znext-solver=globally to be enabled";
        let mut diag = Diag::error(None, message, cx.span(gca.at));
        diag.help("enable all of these features");
        diags.push(diag);
    }
    diags
        .into_iter()
        .map(|mut diag| {
            crate_fact(cx, &mut diag);
            Found::new(diag, Phase::Resolve)
        })
        .collect()
}

/// `maybe_stage_features`: `#![feature]` where rustc is not a nightly, at the first
/// `#![feature]`, with each stable feature it names and, when all are, the suggestion to remove
/// the attribute.
fn on_non_nightly(cx: &Cx<'_, '_>, owners: &[Owner], features: &Features) -> Option<Diag> {
    let named = crate_features(owners);
    let first = owners
        .iter()
        .find(|it| it.target == Target::Crate)
        .and_then(|krate| krate.attrs.iter().find(|it| it.is("feature")))
        .map(|attr| cx.span(attr.in_file(attr.range())))?;
    let message = "`#![feature]` may not be used on the nightly release channel";
    let mut diag = Diag::error(Some("E0554"), message, first.clone());
    let mut all_stable = true;
    for feature in &named {
        let since = features.lang.iter().find(|it| it.name == feature.name).and_then(|it| it.since);
        match since {
            Some(since) => {
                diag.help(format!(
                    "the feature `{}` has been stable since `{since}` and no longer requires an attribute to enable",
                    feature.name
                ));
            }
            None => all_stable = false,
        }
    }
    if all_stable {
        diag.suggestion(first, "remove the attribute", "", Applicability::MachineApplicable);
    }
    Some(diag)
}

/// rustc's `feature_err`: the E0658 error of using `feature`, a language feature, without it,
/// at `span`: its tracking issue, and on a nightly how to enable it and when the compiler was
/// built (not for `rustc_attrs`, which will not be stabilized).
pub(crate) fn feature_err(
    features: &Features,
    feature: &str,
    span: impl Into<MultiSpan>,
    explain: impl Into<String>,
) -> Diag {
    let mut diag = Diag::new(Level::Error, Some(Code::Error("E0658".to_owned())), explain);
    diag.span = span.into();
    feature_notes(&mut diag, features, feature);
    diag
}

/// rustc's `feature_warn`: the warning of a soft gate, `unstable_syntax_pre_expansion` (a
/// future-incompatible warning no lint level changes), of syntax only the parser saw: `cfg`
/// removed it before the gate that errors could. rustc stashes it and writes it at the end,
/// and only when there was no error.
pub(crate) fn feature_warn(features: &Features, feature: &str, span: Span, explain: &str) -> Diag {
    let mut diag = Diag::new(Level::Warning, None, explain);
    diag.span = MultiSpan::of(span);
    feature_notes(&mut diag, features, feature);
    diag.warn("unstable syntax can change at any point in the future, causing a hard error!");
    diag.note("for more information, see issue #154045 <https://github.com/rust-lang/rust/issues/154045>");
    diag
}

/// `add_feature_diagnostics_for_issue` of a language feature.
fn feature_notes(diag: &mut Diag, features: &Features, feature: &str) {
    if let Some(n) = table::issue(feature) {
        diag.note(format!("see issue #{n} <https://github.com/rust-lang/rust/issues/{n}> for more information"));
    }
    if features.nightly {
        diag.help(format!("add `#![feature({feature})]` to the crate attributes to enable"));
        if feature != "rustc_attrs" {
            diag.note(format!("this compiler was built on {BUILT_ON}; consider upgrading it if it is out of date"));
        }
    }
}

/// rustc's `Span::allows_unstable(feature)` of `range` in `file`: the expansion its first
/// token comes from is of a macro defined with `#[allow_internal_unstable(feature)]`. A builtin
/// macro's expansion is the front end's own rendering of what rustc builds without its parser
/// (`format_args!` as `builtin # format_args`), so nothing in it is gated.
pub(crate) fn allows_unstable(cx: &Cx<'_, '_>, file: HirFileId, range: TextRange, feature: &str) -> bool {
    use hir_expand::MacroDefKind;
    if !file.is_macro() {
        return false;
    }
    let db = cx.db;
    let ctxt = file.span_map(db).span_for_range(range).ctx;
    let Some(call) = ctxt.outer_expn(db) else { return false };
    let def = hir_expand::MacroCallId::from(call).loc(db).def;
    let node: SyntaxNode = match def.kind {
        MacroDefKind::Declarative(..) | MacroDefKind::ProcMacro(..) => match def.ast_id() {
            either::Either::Left(id) => id.to_node(db).syntax().clone(),
            either::Either::Right(id) => id.to_node(db).syntax().clone(),
        },
        MacroDefKind::BuiltIn(..)
        | MacroDefKind::BuiltInAttr(..)
        | MacroDefKind::BuiltInDerive(..)
        | MacroDefKind::BuiltInEager(..)
        | MacroDefKind::UnimplementedBuiltIn(..) => return true,
    };
    let Some(owner) = ast::AnyHasAttrs::cast(node) else { return false };
    let cfg = hir::Crate::from(def.krate).cfg(db);
    hir_expand::attrs::expand_cfg_attr(ast::attrs_including_inner(&owner), || cfg, |meta, _| {
        let allowed = matches!(&meta, ast::Meta::TokenTreeMeta(it)
            if it.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "allow_internal_unstable")
                && it.token_tree().is_some_and(|tt| names_feature(&tt, feature)));
        if allowed { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    })
    .is_some()
}

/// Whether the list `tt` of `#[allow_internal_unstable(..)]` has the word `feature`.
fn names_feature(tt: &ast::TokenTree, feature: &str) -> bool {
    tt.syntax().children_with_tokens().any(|it| match it {
        NodeOrToken::Token(token) => {
            token.kind() == SyntaxKind::IDENT && token.text().strip_prefix("r#").unwrap_or(token.text()) == feature
        }
        NodeOrToken::Node(_) => false,
    })
}

/// Records on `diag` what the errors of the crate's features read: the crate root's attributes.
fn crate_fact(cx: &Cx<'_, '_>, diag: &mut Diag) {
    let root = cx.krate.root_module(cx.db);
    diag.fact(Fact::Signature(cx.module_key(root)), "the attributes of the crate root");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustc_bootstrap_decides_the_channel_as_rustc_does() {
        assert!(nightly_build(None, Some("a")));
        assert!(nightly_build(Some("1"), None));
        assert!(!nightly_build(Some("-1"), Some("a")));
        assert!(nightly_build(Some("a,b"), Some("b")));
        assert!(nightly_build(Some("x"), Some("a")));
        assert!(!nightly_build(Some("-1"), None));
    }
}
