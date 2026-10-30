//! rustc's `check_unused_or_stable_features`, as `misc_checking_1` gives it (in
//! `check_mod_unstable_api_usage` of the crate root, after `check_mod_attrs`): `stable_features`
//! of a feature that is stable or partially so, `duplicate_features`, E0635 of a library feature
//! no crate defines (with the feature of a similar name) or one renamed, E0557 of a library
//! feature `#[unstable_removed]` says was removed, the error of an `implied_by` naming no
//! feature, and E0711 of a feature the crate declares twice.
//!
//! A crate's library features are what the stability attributes of a crate with
//! `#![feature(staged_api)]` say, read from its syntax as rustc's attribute parser reads them
//! (an attribute it rejects says nothing). rustc reads them from the metadata of each crate it
//! loads; here each staged dependency's attributes are walked, and only when the crate enables a
//! feature its own attributes don't define, as rustc loads them only then.
//!
//! Follows `compiler/rustc_passes/src/stability.rs` (`check_unused_or_stable_features`,
//! `stability_implications`, `unnecessary_stable_feature_lint`,
//! `unnecessary_partially_stable_feature_lint`, `duplicate_feature_lint`),
//! `compiler/rustc_passes/src/lib_features.rs`, `compiler/rustc_passes/src/diagnostics.rs`
//! (`FeatureStableTwice`, `FeaturePreviouslyDeclared`, `UnknownFeature`, `MisspelledFeature`,
//! `RenamedFeature`, `ImpliedFeatureNotExist`, `FeatureRemoved`, `DuplicateFeature`,
//! `UnnecessaryStableFeature`, `UnnecessaryPartialStableFeature`),
//! `compiler/rustc_attr_parsing/src/attributes/stability.rs` (`parse_stability`,
//! `parse_unstability`, `UnstableRemovedParser`), `compiler/rustc_attr_parsing/src/attributes/
//! util.rs` (`parse_version`), `compiler/rustc_span/src/symbol.rs` (`find_similar`) and
//! `compiler/rustc_span/src/source_map.rs` (`span_extend_to_line`) (MIT or Apache-2.0).

mod unstable;

use hir::InFile;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use span::FileId;
use syntax::{AstNode, AstToken, NodeOrToken, SyntaxKind, T, TextRange, ast};

use super::{Features, crate_fact};
use crate::{
    cx::{Cx, Files},
    lints::{
        edit_distance::find_best_match_for_name,
        syntax::{
            Walk, source_fact,
            attrs::{Attr, Owner, Target, features::table, owners},
        },
    },
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

/// rustc's `env!("CFG_RELEASE")` of the pinned nightly (`harness/toolchain`,
/// `1.101.0-nightly`): what a feature stable since `CURRENT_RUSTC_VERSION` is stable since.
const CFG_RELEASE: &str = "1.101.0-nightly";

/// The `since` a feature stabilized in the release being made is written with.
const VERSION_PLACEHOLDER: &str = "CURRENT_RUSTC_VERSION";

/// `check_unused_or_stable_features` of the crate: its diagnostics in rustc's order, each at
/// `Phase::Items`, the lints at the crate root.
pub(super) fn check(cx: &Cx<'_, '_>, owners: &[Owner], features: &Features) -> Vec<Found> {
    let Some(root) = owners.iter().find(|it| it.target == Target::Crate) else { return Vec::new() };
    let lint_at = root.at();
    let mut found = Vec::new();
    let lint = |found: &mut Vec<Found>, mut diag: Diag| {
        crate_fact(cx, &mut diag);
        found.push(Found::lint(diag, Phase::Items, lint_at));
    };

    let mut lang = FxHashSet::default();
    for enabled in &features.lang {
        if let Some(since) = enabled.since {
            lint(&mut found, stable(cx, enabled.at, &enabled.name, since));
        }
        if !lang.insert(enabled.name.as_str()) {
            lint(&mut found, duplicate(cx, enabled.at, &enabled.name));
        }
    }
    let mut remaining = Remaining::default();
    for enabled in &features.lib {
        if remaining.get(&enabled.name).is_some() {
            lint(&mut found, duplicate(cx, enabled.at, &enabled.name));
        }
        remaining.insert(&enabled.name, enabled.at);
    }
    remaining.swap_remove("libc");
    remaining.swap_remove("test");

    let local = if features.enabled("staged_api") { collect(owners, Some((cx, &mut found))) } else { Library::default() };
    let mut implications = local.implications.clone();
    if !remaining.is_empty() || !implications.is_empty() {
        let libraries: Vec<(hir::Crate, Library)> =
            crates(cx).into_iter().map(|krate| (krate, library_of(cx, krate))).collect();
        let mut all = implications.clone();
        for (_, library) in &libraries {
            all.extend(library.implications.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        let mut walk = Check { cx, remaining: &mut remaining, implications: &mut implications, all: &all, found: &mut found, lint_at };
        walk.features(&local);
        for (_, library) in &libraries {
            if walk.remaining.is_empty() && walk.implications.is_empty() {
                break;
            }
            walk.features(library);
        }
        if !remaining.is_empty() {
            let mut names: Vec<&str> = unstable::UNSTABLE.to_vec();
            for (_, library) in &libraries {
                let start = names.len();
                names.extend(library.features.keys().map(String::as_str));
                names[start..].sort_unstable();
            }
            let removed: Vec<&Removed> = libraries.iter().flat_map(|(_, library)| &library.removed).collect();
            for (feature, at) in std::mem::take(&mut remaining.entries) {
                let span = cx.span(at);
                let mut diag = match removed.iter().find(|it| it.feature == feature) {
                    Some(removed) => {
                        let message = format!("feature `{feature}` has been removed");
                        let mut diag = Diag::error(Some("E0557"), message, span.clone());
                        diag.span_label(span, "feature has been removed");
                        diag.note(format!("removed in {}; see <{}> for more information", removed.since, removed.link));
                        diag.note(removed.reason.clone());
                        diag
                    }
                    None => {
                        let message = format!("unknown feature `{feature}`");
                        let mut diag = Diag::error(Some("E0635"), message, span.clone());
                        if let Some(similar) = similar(&names, &feature) {
                            diag.suggestion_verbose(
                                span,
                                format!("there is a feature with a similar name: `{similar}`"),
                                similar,
                                Applicability::MaybeIncorrect,
                            );
                        }
                        diag
                    }
                };
                crate_fact(cx, &mut diag);
                for (krate, _) in &libraries {
                    let root = krate.root_module(cx.db);
                    diag.fact(blaze_items::Fact::Signature(cx.module_key(root)), "the library features of a dependency");
                }
                found.push(Found::new(diag, Phase::Items));
            }
        }
    }

    let mut left: Vec<(String, String)> = implications.into_iter().collect();
    left.sort_unstable();
    for (implied_by, feature) in left {
        let (_, at) = local
            .features
            .get(&feature)
            .expect("an implication's feature is among the crate's library features: `collect` records both");
        let message = format!("feature `{implied_by}` implying `{feature}` does not exist");
        let mut diag = Diag::error(None, message, cx.span(*at));
        crate_fact(cx, &mut diag);
        found.push(Found::new(diag, Phase::Items));
    }
    found
}

/// What a stability attribute says of the feature it names: rustc's `FeatureStability`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stability {
    /// Stable since the version, as rustc prints it.
    Accepted(String),
    Unstable { old_name: Option<String> },
}

/// A crate's library features: rustc's `LibFeatures`, `stability_implications` and its
/// `#[unstable_removed]`.
#[derive(Default)]
struct Library {
    /// Each feature with what the first attribute naming it says of it and where that is.
    features: FxHashMap<String, (Stability, InFile<TextRange>)>,
    /// Each `implied_by` of the crate's unstable definitions, with the feature it implies.
    implications: FxHashMap<String, String>,
    removed: Vec<Removed>,
}

/// An `#[unstable_removed(feature, reason, link, since)]` of a crate root.
#[derive(Debug)]
struct Removed {
    feature: String,
    reason: String,
    link: String,
    since: String,
}

/// The library features enabled and not yet found among the crates': rustc's
/// `remaining_lib_features`, an `IndexMap` whose order `swap_remove` changes as rustc's does.
struct Remaining<T = InFile<TextRange>> {
    entries: Vec<(String, T)>,
}

impl<T> Default for Remaining<T> {
    fn default() -> Remaining<T> {
        Remaining { entries: Vec::new() }
    }
}

impl<T: Copy> Remaining<T> {
    fn position(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|(it, _)| it == name)
    }

    fn get(&self, name: &str) -> Option<T> {
        self.position(name).map(|it| self.entries[it].1)
    }

    fn insert(&mut self, name: &str, at: T) {
        match self.position(name) {
            Some(it) => self.entries[it].1 = at,
            None => self.entries.push((name.to_owned(), at)),
        }
    }

    fn swap_remove(&mut self, name: &str) -> Option<T> {
        let at = self.position(name)?;
        Some(self.entries.swap_remove(at).1)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// rustc's `check_features`, over one crate's features after another.
struct Check<'a, 'c, 'db> {
    cx: &'a Cx<'c, 'db>,
    remaining: &'a mut Remaining,
    implications: &'a mut FxHashMap<String, String>,
    all: &'a FxHashMap<String, String>,
    found: &'a mut Vec<Found>,
    lint_at: InFile<syntax::SyntaxNodePtr>,
}

impl Check<'_, '_, '_> {
    fn features(&mut self, library: &Library) {
        let mut sorted: Vec<(&String, &Stability)> = library.features.iter().map(|(k, (s, _))| (k, s)).collect();
        sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
        for (feature, stability) in sorted {
            if let Stability::Accepted(since) = stability
                && let Some(at) = self.remaining.get(feature)
            {
                let mut diag = match self.all.get(feature) {
                    Some(implies) => partially_stable(self.cx, at, feature, implies, since),
                    None => stable(self.cx, at, feature, since),
                };
                crate_fact(self.cx, &mut diag);
                self.found.push(Found::lint(diag, Phase::Items, self.lint_at));
            }
            self.remaining.swap_remove(feature);
            self.implications.remove(feature.as_str());
            if let Stability::Unstable { old_name: Some(alias) } = stability
                && let Some(at) = self.remaining.swap_remove(alias)
            {
                let message = format!("feature `{alias}` has been renamed to `{feature}`");
                let mut diag = Diag::error(Some("E0635"), message, self.cx.span(at));
                crate_fact(self.cx, &mut diag);
                self.found.push(Found::new(diag, Phase::Items));
            }
            if self.remaining.is_empty() && self.implications.is_empty() {
                break;
            }
        }
    }
}

/// `unnecessary_stable_feature_lint`.
fn stable(cx: &Cx<'_, '_>, at: InFile<TextRange>, feature: &str, since: &str) -> Diag {
    let since = if since == VERSION_PLACEHOLDER { CFG_RELEASE } else { since };
    let message = format!("the feature `{feature}` has been stable since {since} and no longer requires an attribute to enable");
    Diag::lint("stable_features", message, cx.span(at))
}

/// `unnecessary_partially_stable_feature_lint`.
fn partially_stable(cx: &Cx<'_, '_>, at: InFile<TextRange>, feature: &str, implies: &str, since: &str) -> Diag {
    let span = cx.span(at);
    let message = format!(
        "the feature `{feature}` has been partially stabilized since {since} and is succeeded by the feature `{implies}`"
    );
    let mut diag = Diag::lint("stable_features", message, span.clone());
    diag.suggestion(
        span.clone(),
        format!("if you are using features which are still unstable, change to using `{implies}`"),
        implies,
        Applicability::MaybeIncorrect,
    );
    diag.suggestion(
        line_of(cx, &span),
        "if you are using features which are now stable, remove this line",
        "",
        Applicability::MaybeIncorrect,
    );
    diag
}

/// `duplicate_feature_lint`.
fn duplicate(cx: &Cx<'_, '_>, at: InFile<TextRange>, feature: &str) -> Diag {
    Diag::lint("duplicate_features", format!("the feature `{feature}` has already been enabled"), cx.span(at))
}

/// `span_extend_to_line`: `span` grown to the start and the end of the lines it is on.
fn line_of(cx: &Cx<'_, '_>, span: &Span) -> Span {
    let Some(file) = cx.file_of(span) else { return span.clone() };
    let text = cx.file_text(file);
    let (lo, hi) = (span.lo as usize, span.hi as usize);
    let next = text[hi..].split('\n').next().unwrap_or("");
    let prev = text[..lo].rsplit('\n').next().unwrap_or("");
    Span { lo: (lo - prev.len()) as u32, hi: (hi + next.len()) as u32, ..span.clone() }
}

/// `Symbol::find_similar`: the name that is the feature in lower case, else the nearest one.
fn similar<'a>(names: &[&'a str], feature: &str) -> Option<&'a str> {
    let lowercase = feature.to_lowercase();
    names.iter().copied().find(|it| *it == lowercase).or_else(|| find_best_match_for_name(names, feature, None))
}

/// The crates rustc loads besides the crate (`tcx.crates(())`): its dependencies and theirs, each
/// once, depth first in the order the crate graph lists them.
fn crates(cx: &Cx<'_, '_>) -> Vec<hir::Crate> {
    let mut seen = FxHashSet::default();
    seen.insert(cx.krate);
    let mut order = Vec::new();
    let mut stack: Vec<hir::Crate> = cx.krate.dependencies(cx.db).into_iter().rev().map(|it| it.krate).collect();
    while let Some(krate) = stack.pop() {
        if !seen.insert(krate) {
            continue;
        }
        order.push(krate);
        stack.extend(krate.dependencies(cx.db).into_iter().rev().map(|it| it.krate));
    }
    order
}

/// The names a dependency's files have: none, as its library features are read without a span.
struct NoSpans;

impl Files for NoSpans {
    fn name(&self, file: FileId) -> String {
        unreachable!("the library features of a dependency are read without spans, yet {file:?} was named")
    }
}

/// rustc's `lib_features(krate)` and `stability_implications(krate)` of a dependency: nothing
/// unless it has `#![feature(staged_api)]`.
fn library_of(cx: &Cx<'_, '_>, krate: hir::Crate) -> Library {
    if !krate.is_unstable_feature_enabled(cx.db, &intern::sym::staged_api) {
        return Library::default();
    }
    let dependency = Cx::new(cx.sema, krate, &NoSpans);
    let walk = Walk::of(&dependency);
    collect(&owners(&dependency, &walk), None)
}

/// The groups of stability attributes rustc's attribute parsers have: `StabilityParser`,
/// `ConstStabilityParser`, `BodyStabilityParser`. Each takes the first attribute of the group it
/// can parse; what it parses goes with the node's attributes where its first attribute is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Item,
    Const,
    Body,
}

fn group(name: &str) -> Option<Group> {
    Some(match name {
        "stable" | "unstable" | "rustc_allowed_through_unstable_modules" => Group::Item,
        "rustc_const_stable" | "rustc_const_unstable" | "rustc_promotable" => Group::Const,
        "rustc_default_body_unstable" => Group::Body,
        _ => return None,
    })
}

/// A stability attribute as rustc parses it: `Stability`, `PartialConstStability` or
/// `DefaultBodyStability`.
struct Declared {
    group: Group,
    feature: String,
    level: Level,
    /// The attribute's span (its path's, for the const ones).
    at: InFile<TextRange>,
}

enum Level {
    /// `StabilityLevel::Stable`; no version when `since` is missing or no version
    /// (`StableSince::Err`).
    Stable(Option<String>),
    Unstable { implied_by: Option<String>, old_name: Option<String> },
}

/// Whether rustc's `hir_crate_items(()).definitions()` has what `target` names: what
/// `stability_implications` looks the stability of up.
fn is_definition(target: Target) -> bool {
    !matches!(
        target,
        Target::Expression
            | Target::Statement
            | Target::Arm
            | Target::Param
            | Target::PatField
            | Target::ExprField
            | Target::WherePredicate
            | Target::MacroCall
            | Target::ForLoop
            | Target::While
            | Target::Loop
            | Target::Break
    )
}

/// The library features of a staged crate's attributes (`lib_features`) with the implications
/// of its definitions; with `errors`, the crate's E0711s, as rustc collects its own.
fn collect(owners: &[Owner], mut errors: Option<(&Cx<'_, '_>, &mut Vec<Found>)>) -> Library {
    let mut library = Library::default();
    for owner in owners {
        if owner.target == Target::MacroCall {
            continue;
        }
        if owner.target == Target::Crate {
            library.removed.extend(owner.attrs.iter().filter_map(removed));
        }
        for declared in declared(owner) {
            let stability = match declared.level {
                Level::Stable(None) => None,
                Level::Stable(Some(since)) => Some(Stability::Accepted(since)),
                Level::Unstable { implied_by, old_name } => {
                    if declared.group != Group::Body
                        && is_definition(owner.target)
                        && let Some(implied_by) = implied_by
                    {
                        library.implications.insert(implied_by, declared.feature.clone());
                    }
                    Some(Stability::Unstable { old_name })
                }
            };
            let Some(stability) = stability else { continue };
            let error = match library.features.get(&declared.feature) {
                None => {
                    library.features.insert(declared.feature, (stability, declared.at));
                    continue;
                }
                Some((Stability::Accepted(previous), _)) => match &stability {
                    Stability::Accepted(since) if since != previous => format!(
                        "feature `{}` is declared stable since {since}, but was previously declared stable since {previous}",
                        declared.feature
                    ),
                    Stability::Accepted(_) => continue,
                    Stability::Unstable { .. } => format!(
                        "feature `{}` is declared unstable, but was previously declared stable",
                        declared.feature
                    ),
                },
                Some((Stability::Unstable { .. }, _)) => match &stability {
                    Stability::Accepted(_) => format!(
                        "feature `{}` is declared stable, but was previously declared unstable",
                        declared.feature
                    ),
                    Stability::Unstable { .. } => continue,
                },
            };
            if let Some((cx, found)) = errors.as_mut() {
                let cx: &Cx<'_, '_> = cx;
                let mut diag = Diag::error(Some("E0711"), error, cx.span(declared.at));
                source_fact(cx, &mut diag, owner.at());
                found.push(Found::new(diag, Phase::Items));
            }
        }
    }
    library
}

/// What the stability parsers make of `owner`'s attributes, in the order rustc's parsed
/// attributes are in.
fn declared(owner: &Owner) -> SmallVec<[Declared; 3]> {
    let mut groups: SmallVec<[(usize, Group, Option<Declared>); 3]> = SmallVec::new();
    for (index, attr) in owner.attrs.iter().enumerate() {
        let Some(meta) = attr.meta() else { continue };
        let Some(name) = single_name(meta) else { continue };
        let Some(group) = group(&name) else { continue };
        let slot = match groups.iter().position(|(_, it, _)| *it == group) {
            Some(it) => it,
            None => {
                groups.push((index, group, None));
                groups.len() - 1
            }
        };
        if groups[slot].2.is_none() {
            groups[slot].2 = parse(attr, meta, &name, group);
        }
    }
    groups.into_iter().filter_map(|(_, _, it)| it).collect()
}

/// The attribute's path, when it is one segment.
fn single_name(meta: &ast::Meta) -> Option<String> {
    Some(meta.path()?.as_single_name_ref()?.text().to_string())
}

/// `parse_stability` or `parse_unstability` of the stability attribute `attr`.
fn parse(attr: &Attr, meta: &ast::Meta, name: &str, group: Group) -> Option<Declared> {
    let unstable = match name {
        "stable" | "rustc_const_stable" => false,
        "unstable" | "rustc_const_unstable" | "rustc_default_body_unstable" => true,
        _ => return None,
    };
    let ast::Meta::TokenTreeMeta(list) = meta else { return None };
    let items = name_values(&list.token_tree()?)?;
    let at = attr.in_file(if group == Group::Const { attr.path_range() } else { attr.range() });
    if !unstable {
        let mut feature = None;
        let mut since = None;
        for (key, value) in items {
            let slot = match key.as_str() {
                "feature" => &mut feature,
                "since" => &mut since,
                _ => return None,
            };
            if slot.is_some() {
                return None;
            }
            *slot = Some(value);
        }
        let feature = feature.filter(|it| is_ident(it))?;
        let since = since.and_then(|it| if it == VERSION_PLACEHOLDER { Some(CFG_RELEASE.to_owned()) } else { version(&it) });
        return Some(Declared { group, feature, level: Level::Stable(since), at });
    }
    let (mut feature, mut reason, mut issue, mut implied_by, mut old_name) = (None, None, None, None, None);
    for (key, value) in items {
        let slot = match key.as_str() {
            "feature" => &mut feature,
            "reason" => &mut reason,
            "issue" => &mut issue,
            "implied_by" => &mut implied_by,
            "old_name" => &mut old_name,
            _ => return None,
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(value);
    }
    if let Some(issue) = &issue
        && issue != "none"
    {
        issue.parse::<std::num::NonZero<u32>>().ok()?;
    }
    let feature = feature.filter(|it| is_ident(it))?;
    issue?;
    if table::lang(&feature).is_some_and(|it| it.kind == table::Kind::Accepted) {
        return None;
    }
    Some(Declared { group, feature, level: Level::Unstable { implied_by, old_name }, at })
}

/// `UnstableRemovedParser` of one `#[unstable_removed(..)]`.
fn removed(attr: &Attr) -> Option<Removed> {
    let meta = attr.meta()?;
    if single_name(meta).as_deref() != Some("unstable_removed") {
        return None;
    }
    let ast::Meta::TokenTreeMeta(list) = meta else { return None };
    let (mut feature, mut reason, mut link, mut since) = (None, None, None, None);
    for (key, value) in name_values(&list.token_tree()?)? {
        let slot = match key.as_str() {
            "feature" => &mut feature,
            "since" => &mut since,
            "reason" => &mut reason,
            "link" => &mut link,
            _ => return None,
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(value);
    }
    let (feature, reason, link) = (feature?, reason?, link?);
    Some(Removed { feature, reason, link, since: version(&since?)? })
}

/// `parse_version`, printed as `RustcVersion` prints: `major.minor[.patch]`, no `-` suffix.
fn version(text: &str) -> Option<String> {
    let mut parts = text.split('-');
    let digits = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let mut digits = digits.splitn(3, '.');
    let major: u16 = digits.next()?.parse().ok()?;
    let minor: u16 = digits.next()?.parse().ok()?;
    let patch: u16 = digits.next().unwrap_or("0").parse().ok()?;
    Some(format!("{major}.{minor}.{patch}"))
}

/// An attribute's `key = "value"` items, in order; `None` when one is anything else (rustc's
/// malformed-attribute errors, which are the attribute checks').
fn name_values(tt: &ast::TokenTree) -> Option<SmallVec<[(String, String); 4]>> {
    let mut items = SmallVec::new();
    let mut tokens: SmallVec<[_; 16]> =
        tt.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).skip(1).collect();
    if tokens.last().is_some_and(|it| matches!(it.kind(), T![')'] | T![']'] | T!['}'])) {
        tokens.pop();
    }
    for item in tokens.split(|it| it.kind() == T![,]) {
        match item {
            [] => continue,
            [NodeOrToken::Token(key), NodeOrToken::Token(eq), NodeOrToken::Token(value)]
                if key.kind() == SyntaxKind::IDENT && eq.kind() == T![=] =>
            {
                let value = ast::String::cast(value.clone())?.value().ok()?.into_owned();
                items.push((key.text().trim_start_matches("r#").to_owned(), value));
            }
            _ => return None,
        }
    }
    Some(items)
}

/// rustc_lexer's `is_ident`, for a feature name.
fn is_ident(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|it| it == '_' || unicode_ident::is_xid_start(it))
        && chars.all(unicode_ident::is_xid_continue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_print_as_rustc_prints_them() {
        assert_eq!(version("1.0").as_deref(), Some("1.0.0"));
        assert_eq!(version("1.27.2").as_deref(), Some("1.27.2"));
        assert_eq!(version("1.0.0-beta"), None);
        assert_eq!(version("1"), None);
        assert_eq!(version("1.70000.0"), None);
    }

    #[test]
    fn remaining_swap_removes_as_index_map_does() {
        let mut remaining = Remaining::<usize>::default();
        for (n, name) in ["a", "b", "c", "d"].into_iter().enumerate() {
            remaining.insert(name, n);
        }
        remaining.swap_remove("a");
        let names: Vec<&str> = remaining.entries.iter().map(|(it, _)| it.as_str()).collect();
        assert_eq!(names, ["d", "b", "c"]);
    }

    #[test]
    fn similar_prefers_the_lower_case_name() {
        assert_eq!(similar(&["abc_def", "Abc_Def"], "Abc_Def"), Some("abc_def"));
        assert_eq!(similar(&["const_fn_trait_bound"], "const_fn_trait_bonud"), Some("const_fn_trait_bound"));
    }
}
