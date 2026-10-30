//! The discriminants of an enum: E0370 for one implied past the end of its `repr` type, E0732
//! for explicit discriminants with non-unit variants and no `repr` integer type, E0081 for a
//! value assigned twice, E0084 for a `repr` of an enum with no variants.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/collect.rs` (`check_enum_variant_types`,
//! `EnumDiscriminantOverflowed`) and `check/check.rs` (`check_enum`,
//! `detect_discriminant_duplicate`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::InFile;
use smallvec::SmallVec;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range},
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// A discriminant as rustc's `Discr` holds it: its bits in the enum's `repr` type.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Discr {
    bits: u128,
    signed: bool,
    width: u32,
}

impl Discr {
    fn of(value: i128, signed: bool, width: u32) -> Discr {
        let mask = if width >= 128 { u128::MAX } else { (1u128 << width) - 1 };
        Discr { bits: (value as u128) & mask, signed, width }
    }

    /// The value as rustc writes it: negative in a signed type.
    fn value(self) -> i128 {
        if self.signed && self.width < 128 && self.bits >> (self.width - 1) & 1 == 1 {
            (self.bits | !((1u128 << self.width) - 1)) as i128
        } else {
            self.bits as i128
        }
    }

    fn max(self) -> u128 {
        let bits = if self.signed { self.width - 1 } else { self.width };
        if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 }
    }

    /// The next value, and whether it went past the type's last one (rustc's `checked_add`).
    fn next(self) -> (Discr, bool) {
        let next = Discr::of(self.value().wrapping_add(1), self.signed, self.width);
        (next, self.bits == self.max())
    }

    fn text(self) -> String {
        // alloc: the errors' text, and tests'
        if self.signed { self.value().to_string() } else { self.bits.to_string() }
    }
}

/// A variant with its discriminant, and its node for what rustc's labels point at.
struct Variant<'db> {
    variant: hir::EnumVariant,
    node: InFile<ast::Variant<'db>>,
    discr: Discr,
    /// Whether an expression assigns it, with the literal it is when it is one.
    explicit: Option<Option<u128>>,
    /// The variant its value is counted up from and how many variants later it is, when it is
    /// not assigned.
    relative: usize,
}

/// The span of a variant's name, or of the variant without one.
fn name_span(cx: &Cx<'_, '_>, node: InFile<ast::Variant<'_>>) -> Span {
    match node.value.name() {
        Some(it) => cx.span(node.with_value(it.syntax().text_range())),
        None => cx.span(node.map(|it| it.syntax().text_range())),
    }
}

/// The expression assigning a variant's discriminant.
fn discr_expr(node: ast::Variant<'_>) -> Option<ast::Expr<'_>> {
    node.const_arg().and_then(|it| it.expr())
}

pub(super) fn check(cx: &Cx<'_, '_>, enum_: hir::Enum) -> Vec<Found> {
    // alloc: empty, allocates nothing
    let Some(source) = cx.vsema.source(enum_) else { return Vec::new() };
    let enum_span = || cx.span(source.map(|it| def_range(it.syntax())));
    // alloc: the errors' fact
    let what = || format!("the variants of `{}`", cx.def_path(hir::ModuleDef::Adt(enum_.into())));
    let fact = Fact::Layout(cx.key(ItemId::Adt(hir::Adt::from(enum_).into())));
    // alloc: empty unless there is an error
    let mut found = Vec::new();
    let variants = enum_.variants(cx.db);
    if variants.is_empty() {
        if let Some(hint) = first_repr_hint(source.value) {
            let span = cx.span(source.with_value(hint));
            let mut diag =
                Diag::error(Some("E0084"), "unsupported representation for zero-variant enum", span);
            diag.span_label(enum_span(), "zero-variant enum");
            diag.fact(fact, what());
            found.push(Found::new(diag, Phase::Items));
        }
        return found;
    }
    let needs_repr = needs_repr_int(cx, enum_, &variants, source);
    let Some((signed, width)) = repr_int(cx, enum_) else {
        found.extend(needs_repr.map(|mut diag| {
            // clone: the error's fact
            diag.fact(fact.clone(), what());
            Found::new(diag, Phase::Items)
        }));
        return found;
    };
    let mut discrs: SmallVec<[Variant<'_>; 8]> = SmallVec::new();
    let mut prev: Option<Discr> = None;
    let mut last_explicit = 0;
    for (index, variant) in variants.iter().enumerate() {
        let Some(node) = cx.vsema.source(*variant) else { continue };
        let expr = discr_expr(node.value);
        let wrapped = match prev {
            Some(prev) => prev.next().0,
            None => Discr::of(0, signed, width),
        };
        let discr = match &expr {
            Some(_) => variant.eval(cx.db).map(|it| Discr::of(it, signed, width)).unwrap_or(wrapped),
            None => match prev {
                Some(prev) if prev.next().1 => {
                    let name_span = name_span(cx, node);
                    let mut diag = Diag::error(
                        Some("E0370"),
                        "enum discriminant overflowed",
                        // clone: the error's span
                        name_span.clone(),
                    );
                    // alloc: the error's label
                    diag.span_label(name_span, format!("overflowed on value after {}", prev.text()));
                    let name = variant.name(cx.db);
                    // alloc: the error's note
                    diag.note(format!(
                        "explicitly set `{} = {}` if that is desired outcome",
                        name.as_str(),
                        wrapped.text()
                    ));
                    // clone: the error's fact
                    diag.fact(fact.clone(), what());
                    found.push(Found::new(diag, Phase::Items));
                    wrapped
                }
                _ => wrapped,
            },
        };
        let explicit = expr.map(literal);
        if explicit.is_some() {
            last_explicit = index;
        }
        discrs.push(Variant {
            variant: *variant,
            node,
            discr,
            relative: if explicit.is_some() { 0 } else { index - last_explicit },
            explicit,
        });
        prev = Some(discr);
    }
    if let Some(mut diag) = needs_repr {
        // clone: the error's fact
        diag.fact(fact.clone(), what());
        found.push(Found::new(diag, Phase::Items));
    }
    // alloc: inline up to 16 variants
    let mut order: SmallVec<[usize; 16]> = (0..discrs.len()).collect();
    let mut i = 0;
    while i < order.len() {
        let first = order[i];
        let mut diag: Option<Diag> = None;
        let mut o = i + 1;
        while o < order.len() {
            let other = order[o];
            if discrs[first].discr.bits != discrs[other].discr.bits {
                o += 1;
                continue;
            }
            let diag = diag.get_or_insert_with(|| {
                // alloc: the error's message
                let message = format!(
                    "discriminant value `{}` assigned more than once",
                    discrs[first].discr.text()
                );
                let mut diag = Diag::error(Some("E0081"), message, enum_span());
                label(cx, &mut diag, &discrs, first);
                diag
            });
            label(cx, diag, &discrs, other);
            let last = *order.last().expect("not empty");
            order[o] = last;
            order.pop();
        }
        if let Some(mut diag) = diag {
            // clone: the error's fact
            diag.fact(fact.clone(), what());
            found.push(Found::new(diag, Phase::Items));
        }
        i += 1;
    }
    found
}

/// `check_enum`'s E0732: an enum without a `repr` integer type whose non-unit variant has an
/// explicit discriminant, or that has an explicit discriminant and a non-unit variant.
fn needs_repr_int(
    cx: &Cx<'_, '_>,
    enum_: hir::Enum,
    variants: &[hir::EnumVariant],
    source: InFile<ast::Enum<'_>>,
) -> Option<Diag> {
    if enum_.repr(cx.db).is_some_and(|it| it.int.is_some()) {
        return None;
    }
    // The first non-unit variant, and the first explicit discriminant of a unit and of a
    // non-unit variant.
    let mut non_unit: Option<InFile<ast::Variant<'_>>> = None;
    let mut disr_unit: Option<InFile<ast::Expr<'_>>> = None;
    let mut disr_non_unit: Option<InFile<ast::Expr<'_>>> = None;
    for &variant in variants {
        let Some(node) = cx.vsema.source(variant) else { continue };
        let unit = variant.kind(cx.db) == hir::StructKind::Unit;
        let disr = if unit { &mut disr_unit } else { &mut disr_non_unit };
        if disr.is_none()
            && let Some(expr) = discr_expr(node.value)
        {
            *disr = Some(node.with_value(expr));
        }
        if !unit && non_unit.is_none() {
            non_unit = Some(node);
        }
    }
    if disr_non_unit.is_none() && !(disr_unit.is_some() && non_unit.is_some()) {
        return None;
    }
    let expr_span = |expr: InFile<ast::Expr<'_>>| cx.span(expr.map(|it| it.syntax().text_range()));
    let mut diag = Diag::error(
        Some("E0732"),
        "`#[repr(inttype)]` must be specified for enums with explicit discriminants and non-unit variants",
        cx.span(source.map(|it| def_range(it.syntax()))),
    );
    match (disr_non_unit, disr_unit, non_unit) {
        (Some(at), _, _) => {
            diag.span_label(expr_span(at), "explicit discriminant on non-unit variant specified here");
        }
        (None, Some(at), Some(non_unit)) => {
            diag.span_label(expr_span(at), "explicit discriminant specified here");
            diag.span_label(name_span(cx, non_unit), "non-unit discriminant declared here");
        }
        _ => {}
    }
    Some(diag)
}

/// rustc's `report` in `detect_discriminant_duplicate`: where `discrs[index]` gets its value.
fn label(cx: &Cx<'_, '_>, diag: &mut Diag, discrs: &[Variant<'_>], index: usize) {
    let variant = &discrs[index];
    let value = variant.discr.text();
    let expr_span = || {
        let expr = discr_expr(variant.node.value).expect("an explicit discriminant's expression");
        cx.span(variant.node.with_value(expr.syntax().text_range()))
    };
    let (span, shown) = match variant.explicit {
        // alloc: the error's label
        Some(Some(literal)) if literal != variant.discr.bits => (expr_span(), format!("`{value}` (overflowed from `{literal}`)")),
        // alloc: the error's label
        Some(_) => (expr_span(), format!("`{value}`")),
        // alloc: the error's label
        None if variant.relative == 0 => (name_span(cx, variant.node), format!("`{value}`")),
        None => {
            let start = &discrs[index - variant.relative];
            let distance = variant.relative;
            let variants = if distance > 1 { "variants" } else { "variant" };
            let (name, start_name) = (variant.variant.name(cx.db), start.variant.name(cx.db));
            diag.span_label(
                name_span(cx, start.node),
                // alloc: the error's label
                format!(
                    "discriminant for `{}` incremented from this startpoint (`{}` + {distance} {variants} later => `{}` = {value})",
                    name.as_str(),
                    start_name.as_str(),
                    name.as_str()
                ),
            );
            // alloc: the error's label
            (name_span(cx, variant.node), format!("`{value}`"))
        }
    };
    // alloc: the error's label
    diag.span_label(span, format!("{shown} assigned here"));
}

/// The value of an integer literal, as rustc's check reads one.
fn literal(expr: ast::Expr<'_>) -> Option<u128> {
    let ast::Expr::Literal(literal) = expr else { return None };
    match literal.kind() {
        ast::LiteralKind::IntNumber(it) => it.value().ok(),
        _ => None,
    }
}

/// Whether the enum's discriminants are signed, and their width in bits: its `repr`'s integer
/// type, else `isize`.
fn repr_int(cx: &Cx<'_, '_>, enum_: hir::Enum) -> Option<(bool, u32)> {
    let ty = enum_.variant_body_ty(cx.db);
    let builtin = ty.as_builtin()?;
    let signed = builtin.is_int();
    let size = ty.layout(cx.db).ok()?.size();
    Some((signed, (size * 8) as u32))
}

/// The span of the first hint of the first `#[repr(..)]` of `node`: `i32` of `#[repr(i32)]`.
fn first_repr_hint(node: ast::Enum<'_>) -> Option<TextRange> {
    let attr = node.attrs().find(|it| it.simple_name() == Some("repr"))?;
    let (_, tt) = attr.meta()?.as_simple_call()?;
    let mut start = None;
    let mut end = None;
    let mut depth = 0usize;
    for it in tt.syntax().children_with_tokens() {
        let range = it.text_range();
        match it {
            NodeOrToken::Token(token) if token.kind() == SyntaxKind::L_PAREN && depth == 0 => {
                depth = 1;
                continue;
            }
            NodeOrToken::Token(token) if token.kind() == SyntaxKind::R_PAREN && depth == 1 => break,
            NodeOrToken::Token(token) if token.kind() == SyntaxKind::COMMA => break,
            NodeOrToken::Token(token) if token.kind().is_trivia() => continue,
            _ => {}
        }
        start.get_or_insert(range.start());
        end = Some(range.end());
    }
    Some(TextRange::new(start?, end?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_wrap_in_their_type() {
        let u8 = |it| Discr::of(it, false, 8);
        assert_eq!(u8(257).text(), "1");
        assert_eq!(u8(255).next().0.text(), "0");
        assert!(u8(255).next().1);
        let i8 = |it| Discr::of(it, true, 8);
        assert_eq!(i8(-1).text(), "-1");
        assert_eq!(i8(-2).next().0.text(), "-1");
        assert!(i8(127).next().1);
        assert!(!i8(-1).next().1);
    }
}
