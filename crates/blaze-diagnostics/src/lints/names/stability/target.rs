//! `#[deprecated]` where it does nothing: `useless_deprecated` from the attribute parser's
//! target check ("the `deprecated` attribute cannot be used on .."), and `unused_attributes`
//! from `check_attr` on an associated const or type of a trait impl ("this `#[deprecated]`
//! annotation has no effect"). On a closure, an expression, a statement, a match arm or a
//! macro call the target check's lint is `unused_attributes`, which the attribute checks of
//! `lints::syntax` give.
//!
//! Follows `compiler/rustc_attr_parsing/src/attributes/deprecation.rs` (`ALLOWED_TARGETS`),
//! `compiler/rustc_attr_parsing/src/target_checking.rs` (`check_target`,
//! `allowed_targets_applied`, `filter_targets`), `compiler/rustc_attr_parsing/src/
//! session_diagnostics.rs` (`InvalidTarget`), `compiler/rustc_hir/src/target.rs`
//! (`Target::plural_name`), `compiler/rustc_passes/src/check_attr.rs` (`check_deprecated`) and
//! `compiler/rustc_passes/src/diagnostics.rs` (`DeprecatedAnnotationHasNoEffect`)
//! (MIT or Apache-2.0).

use hir::InFile;
use intern::Symbol;
use syntax::{AstNode, SyntaxNode, SyntaxNodePtr, TextRange, ast};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, SuggestionStyle},
};

/// What an attribute is on, as rustc's `Target` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
    ExternCrate,
    Use,
    Static,
    Const,
    Fn,
    Closure,
    Mod,
    ForeignMod,
    TyAlias,
    Enum,
    Variant,
    Struct,
    Field,
    Union,
    Trait,
    Impl { of_trait: bool },
    Expression,
    Statement,
    Arm,
    AssocConst,
    Method(MethodKind),
    AssocTy,
    ForeignFn,
    ForeignStatic,
    ForeignTy,
    GenericParam(GenericParamKind),
    MacroDef,
    Param,
    PatField,
    ExprField,
    WherePredicate,
    MacroCall,
    Crate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MethodKind {
    Inherent,
    Trait { body: bool },
    TraitImpl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GenericParamKind {
    Type,
    Lifetime,
    Const,
}

impl Target {
    fn plural_name(self) -> &'static str {
        match self {
            Target::ExternCrate => "extern crates",
            Target::Use => "use statements",
            Target::Static => "statics",
            Target::Const => "constants",
            Target::Fn => "functions",
            Target::Closure => "closures",
            Target::Mod => "modules",
            Target::ForeignMod => "foreign modules",
            Target::TyAlias => "type aliases",
            Target::Enum => "enums",
            Target::Variant => "enum variants",
            Target::Struct => "structs",
            Target::Field => "struct fields",
            Target::Union => "unions",
            Target::Trait => "traits",
            Target::Impl { of_trait: false } => "inherent impl blocks",
            Target::Impl { of_trait: true } => "trait impl blocks",
            Target::Expression => "expressions",
            Target::Statement => "statements",
            Target::Arm => "match arms",
            Target::AssocConst => "associated consts",
            Target::Method(MethodKind::Inherent) => "inherent methods",
            Target::Method(MethodKind::Trait { body: false }) => "required trait methods",
            Target::Method(MethodKind::Trait { body: true }) => "provided trait methods",
            Target::Method(MethodKind::TraitImpl) => "trait methods in impl blocks",
            Target::AssocTy => "associated types",
            Target::ForeignFn => "foreign functions",
            Target::ForeignStatic => "foreign statics",
            Target::ForeignTy => "foreign types",
            Target::GenericParam(GenericParamKind::Type) => "type parameters",
            Target::GenericParam(GenericParamKind::Lifetime) => "lifetime parameters",
            Target::GenericParam(GenericParamKind::Const) => "const parameters",
            Target::MacroDef => "macro defs",
            Target::Param => "function params",
            Target::PatField => "pattern fields",
            Target::ExprField => "struct fields",
            Target::WherePredicate => "where predicates",
            Target::MacroCall => "macro calls",
            Target::Crate => "crates",
        }
    }
}

/// `DeprecationParser::ALLOWED_TARGETS`' `Allow`s; `WherePredicate` is its `Error`, every
/// other target its warning.
const ALLOWED: &[Target] = &[
    Target::Fn,
    Target::Mod,
    Target::Struct,
    Target::Enum,
    Target::Union,
    Target::Const,
    Target::Static,
    Target::MacroDef,
    Target::Method(MethodKind::Inherent),
    Target::Method(MethodKind::Trait { body: false }),
    Target::Method(MethodKind::Trait { body: true }),
    Target::TyAlias,
    Target::Use,
    Target::ForeignFn,
    Target::ForeignStatic,
    Target::ForeignTy,
    Target::Field,
    Target::Trait,
    Target::AssocTy,
    Target::AssocConst,
    Target::Variant,
    Target::Impl { of_trait: false },
    Target::Crate,
];

const FUNCTION_LIKE: &[Target] = &[
    Target::Fn,
    Target::Closure,
    Target::ForeignFn,
    Target::Method(MethodKind::Inherent),
    Target::Method(MethodKind::Trait { body: false }),
    Target::Method(MethodKind::Trait { body: true }),
    Target::Method(MethodKind::TraitImpl),
];
const FUNCTION_WITH_BODY_LIKE: &[Target] = &[
    Target::Fn,
    Target::Closure,
    Target::Method(MethodKind::Inherent),
    Target::Method(MethodKind::Trait { body: true }),
    Target::Method(MethodKind::TraitImpl),
];
const METHOD_LIKE: &[Target] = &[
    Target::Method(MethodKind::Inherent),
    Target::Method(MethodKind::Trait { body: false }),
    Target::Method(MethodKind::Trait { body: true }),
    Target::Method(MethodKind::TraitImpl),
];
const IMPL_LIKE: &[Target] = &[Target::Impl { of_trait: false }, Target::Impl { of_trait: true }];
const ADT_LIKE: &[Target] = &[Target::Struct, Target::Enum, Target::Union];

/// A `#[deprecated]` the walk came to, on a node of the crate.
pub(super) struct Attribute {
    pub(super) target: Target,
    /// The node it is on, where its level is decided.
    pub(super) on: InFile<SyntaxNodePtr>,
    /// The attribute's path, and the attribute as written (the `cfg_attr` item it comes from
    /// when it does).
    pub(super) path: InFile<TextRange>,
    pub(super) whole: InFile<TextRange>,
}

impl Attribute {
    /// The `#[deprecated]` `meta` written in `attr` on `on`.
    pub(super) fn new(target: Target, on: InFile<&SyntaxNode>, meta: &ast::Meta, attr: &ast::Attr) -> Option<Attribute> {
        let path = meta.path()?.syntax().text_range();
        let whole = match attr.meta() {
            Some(it) if it.syntax() == meta.syntax() => attr.syntax().text_range(),
            _ => meta.syntax().text_range(),
        };
        Some(Attribute {
            target,
            on: on.map(SyntaxNodePtr::new),
            path: on.with_value(path),
            whole: on.with_value(whole),
        })
    }
}

/// Whether the crate enables `extern_types`, which adds foreign types to the targets the
/// help names.
pub(super) fn extern_types(cx: &Cx<'_, '_>) -> bool {
    cx.krate.is_unstable_feature_enabled(cx.db, &Symbol::intern("extern_types"))
}

/// Whether the target check warns of a `#[deprecated]` on `target` with `useless_deprecated`.
pub(super) fn is_useless(target: Target) -> bool {
    let unused =
        matches!(target, Target::Closure | Target::Expression | Target::Statement | Target::Arm | Target::MacroCall);
    !(ALLOWED.contains(&target) || target == Target::WherePredicate || unused)
}

/// Whether `check_deprecated` can find a `#[deprecated]` on `target` to have no effect.
pub(super) fn may_have_no_effect(target: Target, of_trait_impl: bool) -> bool {
    of_trait_impl && matches!(target, Target::AssocConst | Target::AssocTy | Target::Method(_))
}

/// The target check's `useless_deprecated` of a `#[deprecated]`, where it is a warning.
pub(super) fn useless(cx: &Cx<'_, '_>, attr: &Attribute, extern_types: bool) -> Option<Found> {
    let target = attr.target;
    if !is_useless(target) {
        return None;
    }
    let message = format!("the `deprecated` attribute cannot be used on {}", target.plural_name());
    let mut diag = Diag::lint("useless_deprecated", message, cx.span(attr.path));
    diag.suggestion_with_style(
        vec![(cx.span(attr.whole), String::new())],
        "remove the attribute",
        Applicability::MachineApplicable,
        SuggestionStyle::CompletelyHidden,
    );
    let (applied, only) = applied(target, extern_types);
    let only = if only { "only " } else { "" };
    diag.help(format!("the `deprecated` attribute can {only}be applied to {}", and_list(&applied)));
    diag.warn(
        "this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!",
    );
    super::source_fact(cx, &mut diag, attr.on);
    Some(Found::lint(diag, Phase::Items, attr.on))
}

/// `check_attr`'s `check_deprecated`: `#[deprecated]` on an associated const or type of a
/// trait impl, which the target check allows and nothing reads.
pub(super) fn no_effect(cx: &Cx<'_, '_>, attr: &Attribute, of_trait_impl: bool) -> Option<Found> {
    if !may_have_no_effect(attr.target, of_trait_impl) {
        return None;
    }
    let span = cx.span(attr.whole);
    let mut diag = Diag::lint("unused_attributes", "this `#[deprecated]` annotation has no effect", span.clone());
    diag.suggestion(span, "remove the unnecessary deprecation attribute", "", Applicability::MachineApplicable);
    super::source_fact(cx, &mut diag, attr.on);
    Some(Found::lint(diag, Phase::Items, attr.on))
}

/// `allowed_targets_applied`: the targets named as those `#[deprecated]` can be applied to,
/// groups of two or more that `target` is not in named as one; and whether there is one.
fn applied(target: Target, extern_types: bool) -> (Vec<&'static str>, bool) {
    let mut allowed: Vec<Target> = ALLOWED.iter().copied().filter(|it| extern_types || *it != Target::ForeignTy).collect();
    let mut fake = Vec::new();
    for (group, name) in [
        (FUNCTION_LIKE, "functions"),
        (FUNCTION_WITH_BODY_LIKE, "functions with a body"),
        (METHOD_LIKE, "methods"),
        (IMPL_LIKE, "impl blocks"),
        (ADT_LIKE, "data types"),
    ] {
        if group.contains(&target) || allowed.iter().filter(|it| group.contains(it)).count() < 2 {
            continue;
        }
        allowed.retain(|it| !group.contains(it));
        fake.push(name);
    }
    let mut names: Vec<&'static str> = fake.into_iter().chain(allowed.iter().map(|it| it.plural_name())).collect();
    names.sort_unstable();
    names.dedup();
    let only = names.len() == 1;
    (names, only)
}

/// `DiagArgValue::StrListSepByAnd`.
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [a, b] => format!("{a} and {b}"),
        [init @ .., last] => format!("{}, and {last}", init.join(", ")),
    }
}
