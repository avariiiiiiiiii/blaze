//! `TypeLimits`: literals out of the range of their type (`overflowing_literals`, deny),
//! comparisons a type's limits decide (`unused_comparisons`), comparisons with NaN
//! (`invalid_nan_comparisons`), of wide pointers (`ambiguous_wide_pointer_comparisons`) and of
//! function pointers (`unpredictable_function_pointer_comparisons`).
//!
//! Follows rustc's `compiler/rustc_lint/src/types.rs` (`TypeLimits`, `lint_nan`,
//! `lint_wide_pointer`, `lint_fn_pointer`) and the messages of `compiler/rustc_lint/src/lints.rs`
//! (MIT or Apache-2.0).

mod literal;

use hir_def::{
    AdtId, HasModule as _,
    hir::{BinaryOp, CmpOp, Expr, ExprId, Ordering, UnaryOp},
};
use hir_ty::next_solver::{DbInterner, Ty, TyKind};
use intern::Symbol;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, Ty as _};
use syntax::{AstNode as _, ast};

use super::util::{self, Solver};
use crate::{
    lints::bodies::{BodyCx, LatePass},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(super) fn passes() -> Vec<Box<dyn LatePass>> {
    vec![Box::new(TypeLimits::default())]
}

#[derive(Default)]
struct TypeLimits {
    last_visited_negation: Option<Negation>,
}

/// A `-` expression, and its operand.
struct Negation {
    span: Span,
    negated: ExprId,
}

/// rustc's `hir::BinOpKind` of a comparison.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// rustc's `ComparisonOp`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ComparisonOp {
    BinOp(Cmp),
    Other,
}

impl Cmp {
    fn of(op: CmpOp) -> Cmp {
        match op {
            CmpOp::Eq { negated: false } => Cmp::Eq,
            CmpOp::Eq { negated: true } => Cmp::Ne,
            CmpOp::Ord { ordering: Ordering::Less, strict: true } => Cmp::Lt,
            CmpOp::Ord { ordering: Ordering::Less, strict: false } => Cmp::Le,
            CmpOp::Ord { ordering: Ordering::Greater, strict: true } => Cmp::Gt,
            CmpOp::Ord { ordering: Ordering::Greater, strict: false } => Cmp::Ge,
        }
    }

    fn rev(self) -> Cmp {
        match self {
            Cmp::Lt => Cmp::Gt,
            Cmp::Le => Cmp::Ge,
            Cmp::Gt => Cmp::Lt,
            Cmp::Ge => Cmp::Le,
            it => it,
        }
    }

    fn is_valid<T: PartialOrd>(self, v: T, min: T, max: T) -> bool {
        match self {
            Cmp::Lt => v > min && v <= max,
            Cmp::Le => v >= min && v < max,
            Cmp::Gt => v >= min && v < max,
            Cmp::Ge => v > min && v <= max,
            Cmp::Eq | Cmp::Ne => v >= min && v <= max,
        }
    }
}

/// rustc's `diag_item_cmpop`: the comparison a method of `Ord`, `PartialOrd` or `PartialEq` is.
fn diag_item_cmpop(item: &str) -> Option<ComparisonOp> {
    Some(match item {
        "cmp_ord_max" | "cmp_ord_min" | "ord_cmp_method" | "cmp_partialord_cmp" => ComparisonOp::Other,
        "cmp_partialeq_eq" => ComparisonOp::BinOp(Cmp::Eq),
        "cmp_partialeq_ne" => ComparisonOp::BinOp(Cmp::Ne),
        "cmp_partialord_ge" => ComparisonOp::BinOp(Cmp::Ge),
        "cmp_partialord_gt" => ComparisonOp::BinOp(Cmp::Gt),
        "cmp_partialord_le" => ComparisonOp::BinOp(Cmp::Le),
        "cmp_partialord_lt" => ComparisonOp::BinOp(Cmp::Lt),
        _ => return None,
    })
}

impl LatePass for TypeLimits {
    fn name(&self) -> &'static str {
        "TypeLimits"
    }

    fn check_expr(&mut self, b: &BodyCx<'_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Some(at) = b.expr_node(expr) else { return };
        match b.store().expr(expr) {
            Expr::Literal(_) => {
                let negation = match &self.last_visited_negation {
                    Some(it) if it.negated == expr => Some(it.span.clone()),
                    _ => None,
                };
                literal::lint_literal(b, expr, negation, out);
            }
            Expr::UnaryOp { expr: operand, op: UnaryOp::Neg } => {
                if let Some(span) = util::span(b, expr) {
                    self.last_visited_negation = Some(Negation { span, negated: operand });
                }
            }
            Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::CmpOp(op)) } => {
                let op = Cmp::of(op);
                let Some(span) = util::span(b, expr) else { return };
                if !check_limits(b, op, lhs, rhs) {
                    let diag = Diag::lint("unused_comparisons", "comparison is useless due to type limits", span);
                    out.push(b.lint(diag, Phase::Late, at));
                } else {
                    let cmpop = ComparisonOp::BinOp(op);
                    out.extend(lint_nan(b, expr, &span, op, lhs, rhs));
                    out.extend(lint_wide_pointer(b, expr, &span, cmpop, lhs, rhs));
                    out.extend(lint_fn_pointer(b, expr, &span, cmpop, lhs, rhs));
                }
            }
            Expr::Call { args, .. } if args.len() == 2 => {
                let Some((item, args)) = util::called_item(b, expr) else { return };
                let Some(cmpop) = diag_item_cmpop(&item) else { return };
                let Some(span) = util::span(b, expr) else { return };
                out.extend(lint_wide_pointer(b, expr, &span, cmpop, args[0], args[1]));
                out.extend(lint_fn_pointer(b, expr, &span, cmpop, args[0], args[1]));
            }
            Expr::MethodCall { receiver, args, .. } if args.len() == 1 => {
                let Some(def) = util::method_fn(b, expr) else { return };
                let Some(cmpop) = util::fn_item(b.db(), def).as_ref().map(Symbol::as_str).and_then(diag_item_cmpop) else { return };
                let Some(span) = util::span(b, expr) else { return };
                out.extend(lint_wide_pointer(b, expr, &span, cmpop, receiver, args[0]));
                out.extend(lint_fn_pointer(b, expr, &span, cmpop, receiver, args[0]));
            }
            _ => {}
        }
    }
}

/// The value of the integer literal `expr`, and whether it is suffixed with a signed type or
/// none (rustc's `LitKind::Int` and its `LitIntType`); none for a byte or char literal.
fn int_literal(b: &BodyCx<'_, '_>, expr: ExprId) -> Option<(u128, bool)> {
    let node = util::syntax(b, expr)?;
    let lit = ast::Literal::cast(node.value)?;
    let ast::LiteralKind::IntNumber(number) = lit.kind() else { return None };
    let signed_or_none = match number.suffix() {
        None => true,
        Some(suffix) if suffix.starts_with('i') => true,
        Some(suffix) if suffix.starts_with('u') => false,
        Some(_) => return None,
    };
    Some((number.value().ok()?, signed_or_none))
}

/// rustc's `check_limits`: whether a comparison of an integer with a literal is not decided by
/// the integer's type alone.
fn check_limits(b: &BodyCx<'_, '_>, op: Cmp, l: ExprId, r: ExprId) -> bool {
    let is_lit = |it: ExprId| matches!(b.store().expr(it), Expr::Literal(_));
    let (lit, expr, swap) = if is_lit(l) {
        (l, r, true)
    } else if is_lit(r) {
        (r, l, false)
    } else {
        return true;
    };
    let norm = if swap { op.rev() } else { op };
    match util::ty(b, expr).kind() {
        TyKind::Int(t) => {
            let (min, max) = literal::int_ty_range(t);
            let Some((value, true)) = int_literal(b, lit) else { return true };
            norm.is_valid(value as i128, min, max)
        }
        TyKind::Uint(t) => {
            let (min, max) = literal::uint_ty_range(t);
            let Some((value, _)) = int_literal(b, lit) else { return true };
            norm.is_valid(value, min, max)
        }
        _ => true,
    }
}

/// rustc's `Span::until`: from the start of `span` to the start of `end`.
fn until(span: &Span, end: &Span) -> Span {
    span.with_hi(end.lo.max(span.lo))
}

/// rustc's `lint_nan`.
fn lint_nan(b: &BodyCx<'_, '_>, e: ExprId, e_span: &Span, op: Cmp, l: ExprId, r: ExprId) -> Option<Found> {
    let is_nan = |expr: ExprId| {
        let expr = util::peel_borrows(b, util::peel_blocks(b, expr));
        matches!(util::path_item(b, expr).as_ref().map(Symbol::as_str), Some("f16_nan" | "f32_nan" | "f64_nan" | "f128_nan"))
    };
    const EQ_NE: &str = "incorrect NaN comparison, NaN cannot be directly compared to itself";
    const HELP: &str = "use `f32::is_nan()` or `f64::is_nan()` instead";
    let spans = || {
        let l = util::span(b, l)?;
        let r = util::span(b, r)?;
        Some((util::ancestor_inside(&l, e_span)?, util::ancestor_inside(&r, e_span)?))
    };
    let eq_ne = |parts: &dyn Fn(Span, Span) -> Vec<(Span, String)>| {
        let mut diag = Diag::lint("invalid_nan_comparisons", EQ_NE, e_span.clone());
        match spans() {
            Some((l_span, r_span)) => {
                diag.suggestion_with_style(
                    parts(l_span, r_span),
                    HELP,
                    Applicability::MachineApplicable,
                    SuggestionStyle::ShowAlways,
                );
            }
            None => {
                diag.help(HELP);
            }
        }
        diag
    };
    let ne = op == Cmp::Ne;
    let diag = match op {
        Cmp::Eq | Cmp::Ne if is_nan(l) => eq_ne(&|l_span, r_span| {
            let mut parts = Vec::new();
            if ne {
                parts.push((r_span.shrink_to_lo(), "!".to_owned()));
            }
            parts.push((r_span.shrink_to_hi(), ".is_nan()".to_owned()));
            parts.push((until(&l_span, &r_span), String::new()));
            parts
        }),
        Cmp::Eq | Cmp::Ne if is_nan(r) => eq_ne(&|l_span, r_span| {
            let mut parts = Vec::new();
            if ne {
                parts.push((l_span.shrink_to_lo(), "!".to_owned()));
            }
            parts.push((l_span.shrink_to_hi(), ".is_nan()".to_owned()));
            parts.push((l_span.shrink_to_hi().to(&r_span), String::new()));
            parts
        }),
        Cmp::Lt | Cmp::Le | Cmp::Gt | Cmp::Ge if is_nan(l) || is_nan(r) => {
            Diag::lint("invalid_nan_comparisons", "incorrect NaN comparison, NaN is not orderable", e_span.clone())
        }
        _ => return None,
    };
    Some(b.lint(diag, Phase::Late, b.expr_node(e)?))
}

/// rustc's `lint_wide_pointer`.
fn lint_wide_pointer<'db>(
    b: &BodyCx<'_, 'db>,
    e: ExprId,
    e_span: &Span,
    cmpop: ComparisonOp,
    l: ExprId,
    r: ExprId,
) -> Option<Found> {
    const LINT: &str = "ambiguous_wide_pointer_comparisons";
    const MESSAGE: &str =
        "ambiguous wide pointer comparison, the comparison includes metadata which may not be expected";
    let mut solver: Option<Solver<'db>> = None;
    let mut ptr_unsized = |mut ty: Ty<'db>| -> Option<(usize, &'static str, bool)> {
        let mut refs = 0;
        while let TyKind::Ref(_, inner, _) = ty.kind() {
            ty = inner;
            refs += 1;
        }
        let mut modifiers = "";
        let pointee = match (ty.kind(), ty.as_adt()) {
            (TyKind::RawPtr(pointee, _), _) => pointee,
            (_, Some((adt, args))) if util::diagnostic_item(b.db(), adt.into()).as_deref() == Some("NonNull") => {
                modifiers = ".as_ptr()";
                args.type_at(0)
            }
            _ => return None,
        };
        let solver = solver.get_or_insert_with(|| Solver::of(b));
        (!solver.is_sized(pointee)).then(|| (refs, modifiers, matches!(pointee.kind(), TyKind::Dynamic(..))))
    };
    let l = util::peel_borrows(b, l);
    let r = util::peel_borrows(b, r);
    let (l_refs, l_modifiers, l_dyn) = ptr_unsized(b.infer.type_of_expr_or_pat(l.into())?)?;
    let (r_refs, r_modifiers, r_dyn) = ptr_unsized(b.infer.type_of_expr_or_pat(r.into())?)?;
    let at = b.expr_node(e)?;
    let mut diag = Diag::lint(LINT, MESSAGE, e_span.clone());
    let spans = util::span(b, l)
        .zip(util::span(b, r))
        .and_then(|(l, r)| Some((util::ancestor_inside(&l, e_span)?, util::ancestor_inside(&r, e_span)?)));
    let Some((l_span, r_span)) = spans else {
        diag.help("use explicit `std::ptr::eq` method to compare metadata and addresses");
        diag.help("use `std::ptr::addr_eq` or untyped pointers to only compare their addresses");
        return Some(b.lint(diag, Phase::Late, at));
    };
    let ne = if cmpop == ComparisonOp::BinOp(Cmp::Ne) { "!" } else { "" };
    let is_eq_ne = matches!(cmpop, ComparisonOp::BinOp(Cmp::Eq | Cmp::Ne));
    let is_dyn_comparison = l_dyn && r_dyn;
    let via_method_call = matches!(b.store().expr(e), Expr::MethodCall { .. } | Expr::Call { .. });
    let left = until(&e_span.shrink_to_lo(), &l_span.shrink_to_lo());
    let middle = until(&l_span.shrink_to_hi(), &r_span.shrink_to_lo());
    let right = until(&r_span.shrink_to_hi(), &e_span.shrink_to_hi());
    let deref_left = "*".repeat(l_refs);
    let deref_right = "*".repeat(r_refs);
    let verbose = |diag: &mut Diag, message: &str, parts: Vec<(Span, String)>| {
        diag.suggestion_with_style(parts, message, Applicability::MaybeIncorrect, SuggestionStyle::ShowAlways);
    };
    let eq_parts = |function: &str| {
        vec![
            (left.clone(), format!("{ne}std::ptr::{function}({deref_left}")),
            (middle.clone(), format!("{l_modifiers}, {deref_right}")),
            (right.clone(), format!("{r_modifiers})")),
        ]
    };
    if is_eq_ne {
        verbose(
            &mut diag,
            "use `std::ptr::addr_eq` or untyped pointers to only compare their addresses",
            eq_parts("addr_eq"),
        );
        if !is_dyn_comparison {
            verbose(&mut diag, "use explicit `std::ptr::eq` method to compare metadata and addresses", eq_parts("eq"));
        }
    } else {
        let paren_left = if l_refs != 0 { ")" } else { "" };
        let paren_right = if r_refs != 0 { ")" } else { "" };
        let mut parts = Vec::new();
        if l_refs != 0 {
            parts.push((l_span.shrink_to_lo(), format!("({deref_left}")));
        }
        parts.push((l_span.shrink_to_hi(), format!("{l_modifiers}{paren_left}.cast::<()>()")));
        if r_refs != 0 {
            parts.push((r_span.shrink_to_lo(), format!("({deref_right}")));
        }
        parts.push((r_span.shrink_to_hi(), format!("{r_modifiers}{paren_right}.cast::<()>()")));
        verbose(&mut diag, "use untyped pointers to only compare their addresses", parts);
        let (open, close) = if via_method_call { ("", "") } else { ("(", ")") };
        verbose(
            &mut diag,
            "or expect the lint to compare the pointers metadata and addresses",
            vec![
                (
                    e_span.shrink_to_lo(),
                    format!(r#"{{ #[expect(ambiguous_wide_pointer_comparisons, reason = "...")] {open}"#),
                ),
                (e_span.shrink_to_hi(), format!("{close} }}")),
            ],
        );
    }
    Some(b.lint(diag, Phase::Late, at))
}

/// rustc's `lint_fn_pointer`.
fn lint_fn_pointer<'db>(
    b: &BodyCx<'_, 'db>,
    e: ExprId,
    e_span: &Span,
    cmpop: ComparisonOp,
    l: ExprId,
    r: ExprId,
) -> Option<Found> {
    const LINT: &str = "unpredictable_function_pointer_comparisons";
    let peel_refs = |mut ty: Ty<'db>| {
        let mut refs = 0;
        while let TyKind::Ref(_, inner, _) = ty.kind() {
            ty = inner;
            refs += 1;
        }
        (ty, refs)
    };
    let is_fn = |ty: Ty<'db>| matches!(ty.kind(), TyKind::FnDef(..) | TyKind::FnPtr(..));
    let l = util::peel_borrows(b, l);
    let r = util::peel_borrows(b, r);
    let (l_ty, l_refs) = peel_refs(b.infer.type_of_expr_or_pat(l.into())?);
    let (r_ty, r_refs) = peel_refs(b.infer.type_of_expr_or_pat(r.into())?);
    let at = b.expr_node(e)?;
    let warn = || {
        let mut diag = Diag::lint(
            LINT,
            "function pointer comparisons do not produce meaningful results since their addresses are not guaranteed to be unique",
            e_span.clone(),
        );
        diag.note("the address of the same function can vary between different codegen units");
        diag.note("furthermore, different functions could have the same address after being merged together");
        diag.note("for more information visit <https://doc.rust-lang.org/nightly/core/ptr/fn.fn_addr_eq.html>");
        diag
    };
    if !(is_fn(l_ty) && is_fn(r_ty)) {
        let option = interner(b).lang_items().Option;
        let some_fn = |ty: Ty<'db>| match ty.as_adt() {
            Some((adt, args)) => option.is_some_and(|it| AdtId::from(it) == adt) && is_fn(args.type_at(0)),
            None => false,
        };
        if some_fn(l_ty) && some_fn(r_ty) {
            return Some(b.lint(warn(), Phase::Late, at));
        }
        return None;
    }
    let is_eq_ne = matches!(cmpop, ComparisonOp::BinOp(Cmp::Eq | Cmp::Ne));
    let spans = util::span(b, l)
        .zip(util::span(b, r))
        .and_then(|(l, r)| Some((util::ancestor_inside(&l, e_span)?, util::ancestor_inside(&r, e_span)?)));
    let (true, Some((l_span, r_span))) = (is_eq_ne, spans) else {
        return Some(b.lint(warn(), Phase::Late, at));
    };
    let ne = if cmpop == ComparisonOp::BinOp(Cmp::Ne) { "!" } else { "" };
    let deref_left = "*".repeat(l_refs);
    let deref_right = "*".repeat(r_refs);
    let left = until(&e_span.shrink_to_lo(), &l_span.shrink_to_lo());
    let middle = until(&l_span.shrink_to_hi(), &r_span.shrink_to_lo());
    let right = until(&r_span.shrink_to_hi(), &e_span.shrink_to_hi());
    let close = if matches!(r_ty.kind(), TyKind::FnPtr(..)) {
        ")".to_owned()
    } else {
        let sig = r_ty.callable_sig(interner(b))?;
        let fn_ptr = Ty::new_fn_ptr(interner(b), sig);
        format!(" as {})", util::print_ty(b, r, fn_ptr)?)
    };
    let mut diag = warn();
    diag.suggestion_with_style(
        vec![
            (left, format!("{ne}std::ptr::fn_addr_eq({deref_left}")),
            (middle, format!(", {deref_right}")),
            (right, close),
        ],
        "refactor your code, or use `std::ptr::fn_addr_eq` to suppress the lint",
        Applicability::MaybeIncorrect,
        SuggestionStyle::ShowAlways,
    );
    Some(b.lint(diag, Phase::Late, at))
}

fn interner<'db>(b: &BodyCx<'_, 'db>) -> DbInterner<'db> {
    DbInterner::new_with(b.db(), b.owner.krate(b.db()))
}
