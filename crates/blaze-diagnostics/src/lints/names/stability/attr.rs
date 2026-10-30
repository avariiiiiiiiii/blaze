//! `#[deprecated]`, `#[stable]`, `#[unstable]`, `#[rustc_allowed_through_unstable_modules]`,
//! `#[allow_internal_unstable]` and `#[unstable_feature_bound]` as written on a node, read as
//! rustc's attribute parsers read them: an attribute rustc rejects as malformed gives nothing
//! (its error is the attribute checks'), the first well-formed one of a kind is the one that
//! counts.
//!
//! The attributes are visited in place (`expand_cfg_attr` over the syntax), nothing collected:
//! a node with none costs one look at its children and allocates nothing. Only an attribute
//! that is one of these is parsed, once per definition (`lookup.rs` keeps the result).
//!
//! Follows `compiler/rustc_attr_parsing/src/attributes/deprecation.rs`, `stability.rs`,
//! `allow_unstable.rs` and `util.rs` (`parse_version`), with `compiler/rustc_hir/src/attrs/
//! data_structures.rs` (`Deprecation`, `DeprecatedSince`) and `compiler/rustc_hir/src/
//! stability.rs` (`Stability`, `StabilityLevel`) (MIT or Apache-2.0).

use std::{borrow::Cow, ops::ControlFlow};

use cfg::CfgOptions;
use hir_expand::attrs::{AstKeyValueMetaExt as _, expand_cfg_attr_view};
use intern::Symbol;
use smallvec::SmallVec;
use syntax::{
    NodeOrToken, SyntaxKind, T, TextRange,
    view::{self as ast, AstNode as _, AstToken as _, Node},
};

/// A rustc version, `RustcVersion`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Version {
    pub(super) major: u16,
    pub(super) minor: u16,
    pub(super) patch: u16,
}

/// The version of rustc the pinned nightly is (`harness/toolchain`, `nightly-2026-10-27`:
/// `1.101.0-nightly`): `RustcVersion::CURRENT`, which `since` versions are compared with.
pub(super) const CURRENT: Version = Version { major: 1, minor: 101, patch: 0 };

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// rustc's `parse_version`: `major.minor[.patch]`, no `-` suffix.
pub(super) fn parse_version(text: &str) -> Option<Version> {
    let mut parts = text.split('-');
    let digits = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let mut digits = digits.splitn(3, '.');
    let major = digits.next()?.parse().ok()?;
    let minor = digits.next()?.parse().ok()?;
    let patch = digits.next().unwrap_or("0").parse().ok()?;
    Some(Version { major, minor, patch })
}

/// `DeprecatedSince`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Since {
    RustcVersion(Version),
    /// `TBD`.
    Future,
    /// Anything else, in a crate without `staged_api`.
    NonStandard,
    /// No `since`, in a crate without `staged_api`.
    Unspecified,
    /// A `since` a staged crate can't have.
    Err,
}

/// `Deprecation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Deprecation {
    pub(super) since: Since,
    pub(super) note: Option<String>,
    pub(super) suggestion: Option<String>,
}

impl Deprecation {
    /// Whether the deprecation is in effect now: `deprecated` rather than
    /// `deprecated_in_future`.
    pub(super) fn is_in_effect(&self) -> bool {
        match self.since {
            Since::RustcVersion(since) => since <= CURRENT,
            Since::Future => false,
            Since::NonStandard | Since::Unspecified | Since::Err => true,
        }
    }

    pub(super) fn is_since_rustc_version(&self) -> bool {
        matches!(self.since, Since::RustcVersion(_))
    }
}

/// `StabilityLevel` with the feature it names, interned: checking whether a feature is
/// enabled compares `u32`s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Stability {
    Unstable { feature: Symbol, reason: Option<String>, issue: Option<u32>, implied_by: Option<Symbol> },
    Stable {
        /// `#[rustc_allowed_through_unstable_modules = "..."]`: the deprecation message a path
        /// through an unstable module to the item gets instead of an error.
        allowed_through_unstable_modules: Option<String>,
    },
}

impl Stability {
    pub(super) fn is_unstable(&self) -> bool {
        matches!(self, Stability::Unstable { .. })
    }
}

/// Calls `f` with each attribute of `node` once `cfg_attr` is expanded with `cfg`, outer then
/// inner, with the attribute it is written in, until `f` breaks.
pub(super) fn each<'t, T>(
    node: Node<'t>,
    cfg: &CfgOptions,
    f: impl FnMut(ast::Meta<'t>, ast::Attr<'t>) -> ControlFlow<T>,
) -> Option<T> {
    let owner = ast::AnyHasAttrs::cast(node)?;
    expand_cfg_attr_view(ast::attrs_including_inner(&owner), || cfg, f)
}

/// Whether `meta` is the attribute `name`, a path of one segment: a length check and a
/// compare of the token's bytes, no hashing.
pub(super) fn is(meta: &ast::Meta<'_>, name: &str) -> bool {
    meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == name)
}

/// The `#[deprecated]` of `node`, read as rustc reads it for a crate that is `staged` (has
/// `#![feature(staged_api)]`) or not.
pub(super) fn deprecation(node: Node<'_>, cfg: &CfgOptions, staged: bool) -> Option<Deprecation> {
    each(node, cfg, |meta, _| match is(&meta, "deprecated").then(|| parse_deprecation(&meta, staged)).flatten() {
        Some(it) => ControlFlow::Break(it),
        None => ControlFlow::Continue(()),
    })
}

fn parse_deprecation(meta: &ast::Meta<'_>, staged: bool) -> Option<Deprecation> {
    let (mut since, mut note, mut suggestion) = (None, None, None);
    match meta {
        ast::Meta::PathMeta(_) => {}
        // alloc: the note of a `#[deprecated]` that is written, kept once per definition for the message
        ast::Meta::KeyValueMeta(meta) => note = Some(meta.value_string()?.to_string()),
        ast::Meta::TokenTreeMeta(meta) => {
            let (mut since_at, mut note_at, mut suggestion_at) = (None, None, None);
            for (key, value) in name_values(meta.token_tree()?)? {
                let slot = match key {
                    Key::Since => &mut since_at,
                    Key::Note => &mut note_at,
                    Key::Suggestion => &mut suggestion_at,
                    _ => return None,
                };
                if slot.is_some() {
                    return None;
                }
                *slot = Some(value);
            }
            since = since_at;
            // alloc: the note and suggestion of a `#[deprecated]` that is written, kept once per definition for the message
            (note, suggestion) = (note_at.map(|it| text(it).into_owned()), suggestion_at.map(|it| text(it).into_owned()));
        }
        _ => return None,
    }
    let since = match since.map(text).as_deref() {
        Some("TBD") => Since::Future,
        Some(_) if !staged => Since::NonStandard,
        Some("CURRENT_RUSTC_VERSION") => Since::RustcVersion(CURRENT),
        Some(since) => parse_version(since).map_or(Since::Err, Since::RustcVersion),
        None if staged => Since::Err,
        None => Since::Unspecified,
    };
    if staged && note.is_none() {
        return None;
    }
    Some(Deprecation { since, note, suggestion })
}

/// The stability the attributes of `node` give an item of a staged crate.
pub(super) fn stability(node: Node<'_>, cfg: &CfgOptions) -> Option<Stability> {
    stability_written(node, cfg).map(|(it, _)| it)
}

/// [`stability`] with where its attribute is: the whole `#[...]`, or what a `cfg_attr` expands
/// to (`expand_cfg_attr_item` gives the attribute its item's span).
pub(super) fn stability_written(node: Node<'_>, cfg: &CfgOptions) -> Option<(Stability, TextRange)> {
    let mut stability = None;
    let mut through = None;
    each::<()>(node, cfg, |meta, written| {
        if is(&meta, "stable") || is(&meta, "unstable") {
            if stability.is_none()
                && let ast::Meta::TokenTreeMeta(list) = &meta
                && let Some(items) = list.token_tree().and_then(name_values)
            {
                let range = match meta.syntax().parent() {
                    Some(parent) if parent == written.syntax() => written.syntax().text_range(),
                    _ => meta.syntax().text_range(),
                };
                let parsed = if is(&meta, "unstable") { parse_unstable(items) } else { parse_stable(items) };
                stability = parsed.map(|it| (it, range));
            }
        } else if is(&meta, "rustc_allowed_through_unstable_modules")
            && let ast::Meta::KeyValueMeta(meta) = &meta
        {
            // alloc: the message of a `#[rustc_allowed_through_unstable_modules]` that is written, kept for the deprecation it gives
            through = meta.value_string().map(|it| it.to_string());
        }
        ControlFlow::Continue(())
    });
    if let Some((Stability::Stable { allowed_through_unstable_modules }, _)) = &mut stability {
        *allowed_through_unstable_modules = through;
    }
    stability
}

fn parse_stable(list: Items<'_>) -> Option<Stability> {
    let mut feature = None;
    let mut since = None;
    for (key, value) in list {
        let slot = match key {
            Key::Feature => &mut feature,
            Key::Since => &mut since,
            _ => return None,
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(value);
    }
    feature.filter(|it| is_ident(&text(*it)))?;
    Some(Stability::Stable { allowed_through_unstable_modules: None })
}

fn parse_unstable(list: Items<'_>) -> Option<Stability> {
    let (mut feature, mut reason, mut issue, mut implied_by) = (None, None, None, None);
    let mut _old_name = None;
    for (key, value) in list {
        let slot = match key {
            Key::Feature => &mut feature,
            Key::Reason => &mut reason,
            Key::Issue => &mut issue,
            Key::ImpliedBy => &mut implied_by,
            Key::OldName => &mut _old_name,
            _ => return None,
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(value);
    }
    let feature = feature?;
    let feature = text(feature);
    if !is_ident(&feature) {
        return None;
    }
    let feature = Symbol::intern(&feature);
    let issue = issue?;
    let issue = match &*text(issue) {
        "none" => None,
        issue => Some(issue.parse::<std::num::NonZero<u32>>().ok()?.get()),
    };
    let implied_by = implied_by.map(|it| Symbol::intern(&text(it)));
    // alloc: the reason of an `#[unstable]` that is written, kept once per definition for the E0658 message
    let reason = reason.map(|it| text(it).into_owned());
    Some(Stability::Unstable { feature, reason, issue, implied_by })
}

/// Whether `#[allow_internal_unstable(..)]` on `node` lets a macro's expansion use `feature`.
pub(super) fn allows_internal_unstable(node: Node<'_>, cfg: &CfgOptions, feature: &Symbol) -> bool {
    lists_feature(node, cfg, "allow_internal_unstable", feature)
}

/// Whether an `#[unstable_feature_bound(..)]` on `node` names `feature`.
pub(super) fn unstable_feature_bound(node: Node<'_>, cfg: &CfgOptions, feature: &Symbol) -> bool {
    lists_feature(node, cfg, "unstable_feature_bound", feature)
}

/// Whether an attribute `name(..)` on `node` lists `feature` among its identifiers.
fn lists_feature(node: Node<'_>, cfg: &CfgOptions, name: &str, feature: &Symbol) -> bool {
    let feature = feature.as_str();
    each(node, cfg, |meta, _| {
        if is(&meta, name)
            && let ast::Meta::TokenTreeMeta(meta) = &meta
            && let Some(tt) = meta.token_tree()
            && tt.syntax().children_with_tokens().any(|it| {
                matches!(it, NodeOrToken::Token(token) if token.kind() == SyntaxKind::IDENT
                    && token.text().trim_start_matches("r#") == feature)
            })
        {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })
    .is_some()
}

/// A key of an attribute's `key = "value"` items, of those the stability attributes read;
/// every other is `Other`, which each of them rejects.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Since,
    Note,
    Suggestion,
    Feature,
    Reason,
    Issue,
    ImpliedBy,
    OldName,
    Other,
}

impl Key {
    fn of(text: &str) -> Key {
        match text {
            "since" => Key::Since,
            "note" => Key::Note,
            "suggestion" => Key::Suggestion,
            "feature" => Key::Feature,
            "reason" => Key::Reason,
            "issue" => Key::Issue,
            "implied_by" => Key::ImpliedBy,
            "old_name" => Key::OldName,
            _ => Key::Other,
        }
    }
}

/// An attribute's `key = "value"` items, each value its string literal, unescaped only when
/// read. Most have up to four (`feature`, `reason`, `issue`, `implied_by`), kept inline.
type Items<'t> = SmallVec<[(Key, ast::String<'t>); 4]>;

/// The value of a string literal [`name_values`] checked unescapes: borrowed unless it has
/// an escape.
fn text(value: ast::String<'_>) -> Cow<'_, str> {
    value.value().unwrap_or_default()
}

/// The `key = "value"` items of an attribute's list, each value a string literal; `None` when
/// an item is anything else (rustc's malformed-attribute errors).
fn name_values(tt: ast::TokenTree<'_>) -> Option<Items<'_>> {
    let mut items = Items::new();
    // Inline, no heap: five `key = "value"` items and their commas fit.
    let mut tokens: SmallVec<[ast::Element<'_>; 24]> = SmallVec::new();
    tokens.extend(tt.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).skip(1));
    if tokens.last().is_some_and(|it| it.kind() == T![')']) {
        tokens.pop();
    }
    for item in tokens.split(|it| it.kind() == T![,]) {
        match item {
            [] => continue,
            [NodeOrToken::Token(key), NodeOrToken::Token(eq), NodeOrToken::Token(value)]
                if key.kind() == SyntaxKind::IDENT && eq.kind() == T![=] =>
            {
                let value = ast::String::cast(*value)?;
                if value.value().is_err() {
                    return None;
                }
                items.push((Key::of(key.text().trim_start_matches("r#")), value));
            }
            _ => return None,
        }
    }
    Some(items)
}

/// rustc_lexer's `is_ident`, for a feature name.
fn is_ident(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|it| it == '_' || it.is_alphabetic()) && chars.all(|it| it == '_' || it.is_alphanumeric())
}
