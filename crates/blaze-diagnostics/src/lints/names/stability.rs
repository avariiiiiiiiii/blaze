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
use rustc_hash::FxHashMap;
use syntax::{SyntaxNodePtr, TextRange};

use self::{
    attr::{Deprecation, Since, Stability},
    def::Def,
    lookup::Entry,
    walk::{Container, Use, Walked},
};
use crate::{
    cx::Cx,
    lints::syntax::source_fact,
    lower::{Found, Phase, mutable_ref_binding::BUILT_ON},
    model::{Applicability, Diag},
};

pub(super) fn lints(cx: &Cx<'_, '_>) -> Vec<Found> {
    let walked = walk::walk(cx);
    let mut checker = Checker { cx, walked: &walked, owners: FxHashMap::default(), allowed: FxHashMap::default() };
    let mut found = Vec::new();

    for escape in &walked.escapes {
        let mut diag = Diag::new(
            crate::model::Level::Warning,
            None,
            "`#[macro_escape]` is a deprecated synonym for `#[macro_use]`",
        );
        diag.primary(cx.span(escape.range));
        if escape.inner {
            diag.help("try an outer attribute: `#[macro_use]`");
        }
        source_fact(cx, &mut diag, escape.at);
        found.push(Found::new(diag, Phase::Resolve));
    }
    for mac in &walked.macros {
        found.extend(checker.macro_unstable(mac));
    }
    for mac in &walked.macros {
        found.extend(checker.macro_deprecated(mac));
    }

    let mut items: Vec<((usize, u8, u8, usize), Found)> = Vec::new();
    for deprecated in &walked.attrs {
        if let Some(it) = target::no_effect(cx, &deprecated.attr, deprecated.of_trait_impl) {
            items.push((walked.order(deprecated.owner, 0), it));
        }
    }
    let mut bodies: Vec<(usize, Found)> = Vec::new();
    for (index, used) in walked.uses.iter().enumerate() {
        for it in checker.check(index, used) {
            match used.phase {
                Phase::Bodies => bodies.push((used.owner.map_or(0, |it| it + 1), it)),
                _ => items.push((walked.order(used.owner, 1), it)),
            }
        }
    }
    items.sort_by_key(|(key, _)| *key);
    bodies.sort_by_key(|(key, _)| *key);
    found.extend(items.into_iter().map(|(_, it)| it));
    let useless: Vec<target::Attribute> = walked.attrs.into_iter().map(|it| it.attr).collect();
    found.extend(target::useless(cx, &useless));
    found.extend(bodies.into_iter().map(|(_, it)| it));
    found
}

impl Walked {
    /// Where rustc's checks of the module an item-like is in come to what is reported of it:
    /// the module, `check_mod_attrs` (`0`) before `check_mod_unstable_api_usage` (`1`), the
    /// item-like's list in `hir_module_items`, the item-like.
    fn order(&self, owner: Option<usize>, check: u8) -> (usize, u8, u8, usize) {
        let Some(index) = owner else { return (0, check, 0, 0) };
        let owner = &self.owners[index];
        let list = match owner.container {
            Container::Free => 0,
            Container::Trait => 1,
            Container::Impl { .. } => 2,
            Container::Foreign => 3,
        };
        (owner.module, check, list, index + 1)
    }
}

struct Checker<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    walked: &'a Walked,
    /// `lookup_deprecation_entry` of each item-like, as `hir_get_parent_item` of a use in it.
    owners: FxHashMap<Option<usize>, Option<Entry>>,
    /// Whether the check of each use the parent segments of a path are checked after allowed it.
    allowed: FxHashMap<usize, bool>,
}

impl Checker<'_, '_, '_> {
    /// `check_stability_allow_unstable` of a use: its deprecation (but in a definition
    /// deprecated by the same attribute), then its stability; for a parent segment, its
    /// stability once the path's own check allowed it.
    fn check(&mut self, index: usize, used: &Use) -> Vec<Found> {
        let cx = self.cx;
        let mut found = Vec::new();
        if let Some(main) = used.segment_of {
            if !self.allowed.get(&main).copied().unwrap_or(true) {
                return found;
            }
            let through = match lookup::stability(cx, self.walked.uses[main].def) {
                Some(Stability::Stable { allowed_through_unstable_modules: Some(message) }) => Some(message),
                _ => None,
            };
            if let Some(message) = through {
                if self.unstable(used).is_some() {
                    let depr = Deprecation { since: Since::Unspecified, note: Some(message), suggestion: None };
                    let main = &self.walked.uses[main];
                    found.extend(self.deprecated(used.def, &depr, main.span, None, main.at));
                }
                return found;
            }
        }
        if !used.stability_only
            && !used.derive
            && let Some(entry) = lookup::deprecation(cx, used.def)
        {
            let parent = self.owner_entry(used.owner);
            let skip = parent.as_ref().is_some_and(|it| it.same_origin(&entry));
            if !skip || entry.depr.is_since_rustc_version() {
                let suggestion = used.expr.then_some(used.span);
                found.extend(self.deprecated(used.def, &entry.depr, used.span, suggestion, used.at));
            }
        }
        let unstable = self.unstable(used);
        self.allowed.insert(index, unstable.is_none());
        if let Some(stability) = unstable {
            found.extend(self.unstable_error(used.def, &stability, used.whole, used.at, used.phase));
        }
        found
    }

    fn owner_entry(&mut self, owner: Option<usize>) -> Option<Entry> {
        let cx = self.cx;
        let walked = self.walked;
        self.owners
            .entry(owner)
            .or_insert_with(|| {
                let node = match owner {
                    Some(index) => walked.owners[index].node.clone(),
                    None => cx.krate.root_module(cx.db).definition_source(cx.db).map(|it| it.node()),
                };
                lookup::deprecation_at(cx, node, cx.krate)
            })
            .clone()
    }

    /// The stability of what `used` resolves to when `eval_stability_allow_unstable` denies
    /// it: a public definition of a dependency compiled with `#![feature(staged_api)]`,
    /// unstable, its feature not enabled nor allowed where it is used, nor the `use` it is
    /// in an unstable re-export.
    fn unstable(&self, used: &Use) -> Option<Stability> {
        let cx = self.cx;
        let db = cx.db;
        if used.def.krate(db)? == cx.krate || !used.def.is_public(db) {
            return None;
        }
        let stability = lookup::stability(cx, used.def)?;
        let Stability::Unstable { feature, implied_by, .. } = &stability else { return None };
        let enabled = |feature: &str| cx.krate.is_unstable_feature_enabled(db, &intern::Symbol::intern(feature));
        let allowed = lookup::allows_unstable(cx, used.whole.file_id, used.whole.value, feature)
            || enabled(feature)
            || implied_by.as_deref().is_some_and(enabled)
            || self.is_unstable_reexport(used.owner);
        (!allowed).then_some(stability)
    }

    /// `is_unstable_reexport`: the item-like is a `use` that is itself unstable.
    fn is_unstable_reexport(&self, owner: Option<usize>) -> bool {
        let cx = self.cx;
        let Some(owner) = owner else { return false };
        let node = &self.walked.owners[owner].node;
        syntax::ast::Use::can_cast(node.value.kind())
            && lookup::staged(cx, cx.krate)
            && lookup::stability_at(cx, node.clone(), cx.krate).is_some_and(|it| it.is_unstable())
    }

    /// `deprecation_message` and `deprecation_suggestion` of a use of `def`, at `span`, its
    /// level decided at `at`.
    fn deprecated(
        &self,
        def: Def,
        depr: &Deprecation,
        span: InFile<TextRange>,
        suggestion: Option<InFile<TextRange>>,
        at: InFile<SyntaxNodePtr>,
    ) -> Option<Found> {
        let cx = self.cx;
        let kind = def.descr(cx.db);
        let path = def::path(cx, def)?;
        let mut diag = deprecation_diag(cx, depr, kind, &path, span, suggestion);
        diag.fact_opt(self.definition_fact(def), format!("the attributes of `{path}`"));
        source_fact(cx, &mut diag, at);
        let phase = match self.walked.uses.iter().find(|it| it.at == at && it.span == span) {
            Some(it) => it.phase,
            None => Phase::Items,
        };
        Some(Found::lint(diag, phase, at))
    }

    /// `report_unstable` of a use of `def` whose stability is `stability`.
    fn unstable_error(
        &self,
        def: Def,
        stability: &Stability,
        span: InFile<TextRange>,
        at: InFile<SyntaxNodePtr>,
        phase: Phase,
    ) -> Option<Found> {
        let cx = self.cx;
        let mut diag = unstable_diag(cx, stability, span)?;
        if let Some(path) = def::path(cx, def) {
            diag.fact_opt(self.definition_fact(def), format!("the stability of `{path}`"));
        }
        source_fact(cx, &mut diag, at);
        Some(Found::new(diag, phase))
    }

    fn definition_fact(&self, def: Def) -> Option<Fact> {
        let cx = self.cx;
        match def.item(cx.db) {
            ModuleDef::Module(module) => Some(Fact::Signature(cx.module_key(module))),
            _ => def.item_id(cx.db).map(|it| Fact::Signature(cx.key(it))),
        }
    }

    /// `check_stability_and_deprecation`'s stability of a macro: its own attribute, checked
    /// in every crate.
    fn macro_unstable(&self, mac: &walk::MacroUse) -> Option<Found> {
        let cx = self.cx;
        let db = cx.db;
        let def = Def::Item(ModuleDef::Macro(mac.mac));
        let krate = def.krate(db)?;
        if !lookup::staged(cx, krate) {
            return None;
        }
        let node = def.source(cx)?;
        let stability = attr::stability(&lookup::own_attrs(cx, &node, krate))?;
        let Stability::Unstable { feature, implied_by, .. } = &stability else { return None };
        let allowed = |feature: &str| {
            cx.krate.is_unstable_feature_enabled(db, &intern::Symbol::intern(feature))
                || lookup::allows_unstable(cx, mac.span.file_id, mac.span.value, feature)
        };
        if allowed(feature) || implied_by.as_deref().is_some_and(allowed) {
            return None;
        }
        let mut diag = unstable_diag(cx, &stability, mac.span)?;
        diag.fact_opt(self.definition_fact(def), format!("the stability of `{}`", mac.path));
        source_fact(cx, &mut diag, mac.at);
        Some(Found::new(diag, Phase::Resolve))
    }

    /// `early_report_macro_deprecation`: a macro's own `#[deprecated]`, the path as written.
    fn macro_deprecated(&self, mac: &walk::MacroUse) -> Option<Found> {
        let cx = self.cx;
        if mac.derive {
            return None;
        }
        let def = Def::Item(ModuleDef::Macro(mac.mac));
        let krate = def.krate(cx.db)?;
        let node = def.source(cx)?;
        let depr = attr::deprecation(&lookup::own_attrs(cx, &node, krate), lookup::staged(cx, krate))?;
        let mut diag = deprecation_diag(cx, &depr, "macro", &mac.path, mac.span, Some(mac.span));
        diag.fact_opt(self.definition_fact(def), format!("the attributes of `{}`", mac.path));
        source_fact(cx, &mut diag, mac.at);
        Some(Found::lint(diag, Phase::Resolve, mac.at))
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
