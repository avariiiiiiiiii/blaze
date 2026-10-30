//! `#[deprecated]`, `#[stable]`, `#[unstable]`, `#[rustc_allowed_through_unstable_modules]` and
//! `#[allow_internal_unstable]` as written on a node, read as rustc's attribute parsers read
//! them: an attribute rustc rejects as malformed gives nothing (its error is the attribute
//! checks'), the first well-formed one of a kind is the one that counts.
//!
//! The attributes are visited in place (`expand_cfg_attr` over the syntax), nothing collected:
//! a node with none costs one look at its children and allocates nothing. Only an attribute
//! that is one of these is parsed, once per definition (`lookup.rs` keeps the result).
//!
//! Follows `compiler/rustc_attr_parsing/src/attributes/deprecation.rs`, `stability.rs`,
//! `allow_unstable.rs` and `util.rs` (`parse_version`), with `compiler/rustc_hir/src/attrs/
//! data_structures.rs` (`Deprecation`, `DeprecatedSince`) and `compiler/rustc_hir/src/
//! stability.rs` (`Stability`, `StabilityLevel`) (MIT or Apache-2.0).

use std::ops::ControlFlow;

use cfg::CfgOptions;
use hir_expand::attrs::{AstKeyValueMetaExt as _, expand_cfg_attr};
use intern::Symbol;
use smallvec::SmallVec;
use syntax::{AstToken, NodeOrToken, SyntaxKind, SyntaxNode, T, ast};

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
pub(super) fn each<T>(
    node: &SyntaxNode,
    cfg: &CfgOptions,
    f: impl FnMut(ast::Meta, ast::Attr) -> ControlFlow<T>,
) -> Option<T> {
    let owner = ast::AnyHasAttrs::cast(node.clone())?;
    expand_cfg_attr(ast::attrs_including_inner(&owner), || cfg, f)
}

/// Whether `meta` is the attribute `name`, a path of one segment: a length check and a
/// compare of the token's bytes, no hashing.
pub(super) fn is(meta: &ast::Meta, name: &str) -> bool {
    meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == name)
}

/// The `#[deprecated]` of `node`, read as rustc reads it for a crate that is `staged` (has
/// `#![feature(staged_api)]`) or not.
pub(super) fn deprecation(node: &SyntaxNode, cfg: &CfgOptions, staged: bool) -> Option<Deprecation> {
    each(node, cfg, |meta, _| match is(&meta, "deprecated").then(|| parse_deprecation(&meta, staged)).flatten() {
        Some(it) => ControlFlow::Break(it),
        None => ControlFlow::Continue(()),
    })
}

fn parse_deprecation(meta: &ast::Meta, staged: bool) -> Option<Deprecation> {
    let mut since = None;
    let mut note = None;
    let mut suggestion = None;
    match meta {
        ast::Meta::PathMeta(_) => {}
        ast::Meta::KeyValueMeta(meta) => note = Some(meta.value_string()?.to_string()),
        ast::Meta::TokenTreeMeta(meta) => {
            for (key, value) in name_values(&meta.token_tree()?)? {
                let slot = match key.as_str() {
                    "since" => &mut since,
                    "note" => &mut note,
                    "suggestion" => &mut suggestion,
                    _ => return None,
                };
                if slot.is_some() {
                    return None;
                }
                *slot = Some(value);
            }
        }
        _ => return None,
    }
    let since = match since {
        Some(since) if since == "TBD" => Since::Future,
        Some(_) if !staged => Since::NonStandard,
        Some(since) if since == "CURRENT_RUSTC_VERSION" => Since::RustcVersion(CURRENT),
        Some(since) => parse_version(&since).map_or(Since::Err, Since::RustcVersion),
        None if staged => Since::Err,
        None => Since::Unspecified,
    };
    if staged && note.is_none() {
        return None;
    }
    Some(Deprecation { since, note, suggestion })
}

/// The stability the attributes of `node` give an item of a staged crate.
pub(super) fn stability(node: &SyntaxNode, cfg: &CfgOptions) -> Option<Stability> {
    let mut stability = None;
    let mut through = None;
    each::<()>(node, cfg, |meta, _| {
        if is(&meta, "stable") || is(&meta, "unstable") {
            if stability.is_none()
                && let ast::Meta::TokenTreeMeta(list) = &meta
                && let Some(items) = list.token_tree().and_then(|it| name_values(&it))
            {
                stability = if is(&meta, "unstable") { parse_unstable(items) } else { parse_stable(items) };
            }
        } else if is(&meta, "rustc_allowed_through_unstable_modules")
            && let ast::Meta::KeyValueMeta(meta) = &meta
        {
            through = meta.value_string().map(|it| it.to_string());
        }
        ControlFlow::Continue(())
    });
    if let Some(Stability::Stable { allowed_through_unstable_modules }) = &mut stability {
        *allowed_through_unstable_modules = through;
    }
    stability
}

fn parse_stable(list: Items) -> Option<Stability> {
    let mut feature = None;
    let mut since = None;
    for (key, value) in list {
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
    feature.filter(|it| is_ident(it))?;
    Some(Stability::Stable { allowed_through_unstable_modules: None })
}

fn parse_unstable(list: Items) -> Option<Stability> {
    let (mut feature, mut reason, mut issue, mut implied_by) = (None, None, None, None);
    let mut _old_name = None;
    for (key, value) in list {
        let slot = match key.as_str() {
            "feature" => &mut feature,
            "reason" => &mut reason,
            "issue" => &mut issue,
            "implied_by" => &mut implied_by,
            "old_name" => &mut _old_name,
            _ => return None,
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(value);
    }
    let feature = Symbol::intern(&feature.filter(|it| is_ident(it))?);
    let issue = match issue?.as_str() {
        "none" => None,
        issue => Some(issue.parse::<std::num::NonZero<u32>>().ok()?.get()),
    };
    let implied_by = implied_by.map(|it| Symbol::intern(&it));
    Some(Stability::Unstable { feature, reason, issue, implied_by })
}

/// Whether `#[allow_internal_unstable(..)]` on `node` lets a macro's expansion use `feature`.
pub(super) fn allows_internal_unstable(node: &SyntaxNode, cfg: &CfgOptions, feature: &Symbol) -> bool {
    let feature = feature.as_str();
    each(node, cfg, |meta, _| {
        if is(&meta, "allow_internal_unstable")
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

/// An attribute's `key = "value"` items. Most have up to four (`feature`, `reason`, `issue`,
/// `implied_by`), kept inline.
type Items = SmallVec<[(String, String); 4]>;

/// The `key = "value"` items of an attribute's list, each value a string literal; `None` when
/// an item is anything else (rustc's malformed-attribute errors).
fn name_values(tt: &ast::TokenTree) -> Option<Items> {
    let mut items = Items::new();
    let mut tokens: SmallVec<[_; 16]> =
        tt.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).skip(1).collect();
    if tokens.last().is_some_and(|it| it.kind() == T![')']) {
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
    chars.next().is_some_and(|it| it == '_' || it.is_alphabetic()) && chars.all(|it| it == '_' || it.is_alphanumeric())
}
