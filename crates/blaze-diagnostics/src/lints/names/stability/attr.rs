//! `#[deprecated]`, `#[stable]`, `#[unstable]`, `#[rustc_allowed_through_unstable_modules]` and
//! `#[allow_internal_unstable]` as written on a node, read as rustc's attribute parsers read
//! them: an attribute rustc rejects as malformed gives nothing (its error is the attribute
//! checks'), the first well-formed one of a kind is the one that counts.
//!
//! Follows `compiler/rustc_attr_parsing/src/attributes/deprecation.rs`, `stability.rs`,
//! `allow_unstable.rs` and `util.rs` (`parse_version`), with `compiler/rustc_hir/src/attrs/
//! data_structures.rs` (`Deprecation`, `DeprecatedSince`) and `compiler/rustc_hir/src/
//! stability.rs` (`Stability`, `StabilityLevel`) (MIT or Apache-2.0).

use std::ops::ControlFlow;

use cfg::CfgOptions;
use hir_expand::attrs::{AstKeyValueMetaExt as _, expand_cfg_attr};
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
    /// `since = "TBD"`.
    Future,
    /// Any other `since` of a crate without `#![feature(staged_api)]`.
    NonStandard,
    /// No `since`.
    Unspecified,
    /// A `since` of a staged crate that is no version.
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

/// `StabilityLevel` with the feature it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Stability {
    Unstable { feature: String, reason: Option<String>, issue: Option<u32>, implied_by: Option<String> },
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

/// The attributes of `node` once `cfg_attr` is expanded with `cfg`, outer then inner, each
/// with the attribute it is written in.
pub(super) fn attrs(node: &SyntaxNode, cfg: &CfgOptions) -> Vec<(ast::Meta, ast::Attr)> {
    let Some(owner) = ast::AnyHasAttrs::cast(node.clone()) else { return Vec::new() };
    let mut attrs = Vec::new();
    expand_cfg_attr::<()>(
        ast::attrs_including_inner(&owner),
        || cfg,
        |meta, attr| {
            attrs.push((meta, attr));
            ControlFlow::Continue(())
        },
    );
    attrs
}

/// Whether `meta` is the attribute `name`, a path of one segment.
pub(super) fn is(meta: &ast::Meta, name: &str) -> bool {
    meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == name)
}

/// The `#[deprecated]` of `attrs`, read as rustc reads it for a crate that is `staged` (has
/// `#![feature(staged_api)]`) or not.
pub(super) fn deprecation(attrs: &[(ast::Meta, ast::Attr)], staged: bool) -> Option<Deprecation> {
    attrs.iter().filter(|(meta, _)| is(meta, "deprecated")).find_map(|(meta, _)| parse_deprecation(meta, staged))
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

/// The stability `attrs` give an item of a staged crate.
pub(super) fn stability(attrs: &[(ast::Meta, ast::Attr)]) -> Option<Stability> {
    let mut stability = None;
    let mut through = None;
    for (meta, _) in attrs {
        if is(meta, "stable") || is(meta, "unstable") {
            if stability.is_some() {
                continue;
            }
            let ast::Meta::TokenTreeMeta(meta) = meta else { continue };
            let Some(list) = meta.token_tree().and_then(|it| name_values(&it)) else { continue };
            let unstable = meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "unstable");
            stability = if unstable { parse_unstable(list) } else { parse_stable(list) };
        } else if is(meta, "rustc_allowed_through_unstable_modules")
            && let ast::Meta::KeyValueMeta(meta) = meta
        {
            through = meta.value_string().map(|it| it.to_string());
        }
    }
    if let Some(Stability::Stable { allowed_through_unstable_modules }) = &mut stability {
        *allowed_through_unstable_modules = through;
    }
    stability
}

fn parse_stable(list: Vec<(String, String)>) -> Option<Stability> {
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

fn parse_unstable(list: Vec<(String, String)>) -> Option<Stability> {
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
    let feature = feature.filter(|it| is_ident(it))?;
    let issue = match issue?.as_str() {
        "none" => None,
        issue => Some(issue.parse::<std::num::NonZero<u32>>().ok()?.get()),
    };
    Some(Stability::Unstable { feature, reason, issue, implied_by })
}

/// The features `#[allow_internal_unstable(..)]` of `attrs` lets a macro's expansion use.
pub(super) fn allow_internal_unstable(attrs: &[(ast::Meta, ast::Attr)]) -> Vec<String> {
    let mut features = Vec::new();
    for (meta, _) in attrs {
        if !is(meta, "allow_internal_unstable") {
            continue;
        }
        let ast::Meta::TokenTreeMeta(meta) = meta else { continue };
        let Some(tt) = meta.token_tree() else { continue };
        for token in tt.syntax().children_with_tokens() {
            if let NodeOrToken::Token(token) = token
                && token.kind() == SyntaxKind::IDENT
            {
                features.push(token.text().trim_start_matches("r#").to_owned());
            }
        }
    }
    features
}

/// The `key = "value"` items of an attribute's list, each value a string literal; `None` when
/// an item is anything else (rustc's malformed-attribute errors).
fn name_values(tt: &ast::TokenTree) -> Option<Vec<(String, String)>> {
    let mut items = Vec::new();
    let mut tokens = tt
        .syntax()
        .children_with_tokens()
        .filter(|it| !it.kind().is_trivia())
        .skip(1)
        .collect::<Vec<_>>();
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
