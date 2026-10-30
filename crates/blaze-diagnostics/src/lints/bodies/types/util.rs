//! What the passes of values, numbers, pointers and casts share: the item a call, a method
//! call or a path names and its diagnostic item, rustc's peeling of blocks and borrows, the
//! trait queries of a body's types, and spans as rustc's `find_ancestor_inside` and
//! `in_external_macro` take them.
//!
//! Follows rustc's `compiler/rustc_hir/src/hir.rs` (`Expr::peel_blocks`, `peel_borrows`),
//! `compiler/rustc_span/src/lib.rs` (`Span::find_ancestor_inside`, `from_expansion`),
//! `compiler/rustc_middle/src/lint.rs` (`in_external_macro`) and
//! `compiler/rustc_passes/src/diagnostic_items.rs` (MIT or Apache-2.0).

use std::ops::ControlFlow;

use base_db::Crate;
use hir::{InFile, db::HirDatabase};
use hir_def::{
    AdtId, AttrDefId, CallableDefId, FunctionId, HasModule, Lookup,
    expr_store::scope::ExprScopes,
    hir::{Expr, ExprId, Literal},
    resolver::{ValueNs, resolver_for_scope},
    src::HasSource,
};
use hir_expand::attrs::{AstKeyValueMetaExt as _, AstPathExt as _, expand_cfg_attr};
use hir_ty::{
    method_resolution::CandidateId,
    next_solver::{
        CallableIdWrapper, DbInterner, ParamEnv, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
};
use rustc_type_ir::inherent::IntoKind;
use syntax::{
    AstNode, SyntaxNode,
    ast::{self, HasAttrs},
};

use crate::{lints::bodies::BodyCx, model::Span};

/// The name `#[rustc_diagnostic_item = "name"]` gives `def`, its `cfg_attr`s expanded: what
/// rustc's `get_diagnostic_name` gives (`diagnostic_items.rs`, `observe_item`).
pub(crate) fn diagnostic_item(db: &dyn HirDatabase, def: AttrDefId) -> Option<String> {
    let (krate, attrs): (Crate, Vec<ast::Attr>) = match def {
        AttrDefId::FunctionId(it) => (it.krate(db), it.lookup(db).source(db).value.attrs().collect()),
        AttrDefId::ConstId(it) => (it.krate(db), it.lookup(db).source(db).value.attrs().collect()),
        AttrDefId::StaticId(it) => (it.krate(db), it.lookup(db).source(db).value.attrs().collect()),
        AttrDefId::TraitId(it) => (it.krate(db), it.lookup(db).source(db).value.attrs().collect()),
        AttrDefId::TypeAliasId(it) => (it.krate(db), it.lookup(db).source(db).value.attrs().collect()),
        AttrDefId::AdtId(AdtId::StructId(it)) => {
            (it.krate(db), it.lookup(db).source(db).value.attrs().collect())
        }
        AttrDefId::AdtId(AdtId::EnumId(it)) => (it.krate(db), it.lookup(db).source(db).value.attrs().collect()),
        AttrDefId::AdtId(AdtId::UnionId(it)) => {
            (it.krate(db), it.lookup(db).source(db).value.attrs().collect())
        }
        _ => return None,
    };
    let cfg = krate.cfg_options(db);
    expand_cfg_attr(attrs.into_iter(), || cfg, |meta, _| match meta {
        ast::Meta::KeyValueMeta(attr) if attr.path().is1("rustc_diagnostic_item") => match attr.value_string() {
            Some(name) => ControlFlow::Break(name.to_string()),
            None => ControlFlow::Continue(()),
        },
        _ => ControlFlow::Continue(()),
    })
}

/// [`diagnostic_item`] of a function.
pub(crate) fn fn_item(db: &dyn HirDatabase, def: FunctionId) -> Option<String> {
    diagnostic_item(db, AttrDefId::FunctionId(def))
}

/// The function `call` calls when its callee is a path naming one (rustc's `qpath_res` of the
/// callee, `opt_def_id`), and the callee.
pub(crate) fn called_fn(b: &BodyCx<'_, '_>, call: ExprId) -> Option<(FunctionId, ExprId)> {
    let Expr::Call { callee, .. } = &b.store()[call] else { return None };
    let Expr::Path(_) = &b.store()[*callee] else { return None };
    match b.infer.expr_ty(*callee).kind() {
        TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(def)), _) => Some((def, *callee)),
        _ => None,
    }
}

/// The diagnostic item of the function `call` calls through a path, with its arguments.
pub(crate) fn called_item<'a>(b: &'a BodyCx<'_, '_>, call: ExprId) -> Option<(String, &'a [ExprId])> {
    let (def, _) = called_fn(b, call)?;
    let name = fn_item(b.db(), def)?;
    Some((name, call_args(b, call)?))
}

/// The arguments of the call `call`, if it is one.
pub(crate) fn call_args<'a>(b: &'a BodyCx<'_, '_>, call: ExprId) -> Option<&'a [ExprId]> {
    match &b.store()[call] {
        Expr::Call { args, .. } => Some(args),
        _ => None,
    }
}

/// The function a method call resolved to (rustc's `type_dependent_def_id`): the trait's
/// method for a method of a trait.
pub(crate) fn method_fn(b: &BodyCx<'_, '_>, expr: ExprId) -> Option<FunctionId> {
    let Expr::MethodCall { .. } = &b.store()[expr] else { return None };
    b.infer.method_resolution(expr).map(|(def, _)| def)
}

/// What the path `expr` names in the value namespace (rustc's `qpath_res`): an associated item
/// as type checking resolved it, else as the scope of `expr` resolves it.
pub(crate) fn path_value(b: &BodyCx<'_, '_>, expr: ExprId) -> Option<ValueNs> {
    let Expr::Path(path) = &b.store()[expr] else { return None };
    if let Some((assoc, _)) = b.infer.assoc_resolutions_for_expr(expr) {
        return Some(match assoc {
            CandidateId::FunctionId(it) => ValueNs::FunctionId(it),
            CandidateId::ConstId(it) => ValueNs::ConstId(it),
        });
    }
    let db = b.db();
    let scope = ExprScopes::of(db, b.owner).scope_for(expr);
    resolver_for_scope(db, b.owner, scope).resolve_path_in_value_ns_fully(db, path, b.store().expr_path_hygiene(expr))
}

/// The diagnostic item the path `expr` names (`f32::NAN`, `std::ptr::null`).
pub(crate) fn path_item(b: &BodyCx<'_, '_>, expr: ExprId) -> Option<String> {
    let def = match path_value(b, expr)? {
        ValueNs::FunctionId(it) => AttrDefId::FunctionId(it),
        ValueNs::ConstId(it) => AttrDefId::ConstId(it),
        ValueNs::StaticId(it) => AttrDefId::StaticId(it),
        ValueNs::StructId(it) => AttrDefId::AdtId(AdtId::StructId(it)),
        _ => return None,
    };
    diagnostic_item(b.db(), def)
}

/// rustc's `Expr::peel_blocks`: through each block with a tail to its tail.
pub(crate) fn peel_blocks(b: &BodyCx<'_, '_>, mut expr: ExprId) -> ExprId {
    while let Expr::Block { tail: Some(tail), .. } = &b.store()[expr] {
        expr = *tail;
    }
    expr
}

/// rustc's `Expr::peel_borrows`: through each `&`, `&mut` and `&raw` to what it borrows.
pub(crate) fn peel_borrows(b: &BodyCx<'_, '_>, mut expr: ExprId) -> ExprId {
    while let Expr::Ref { expr: inner, .. } = &b.store()[expr] {
        expr = *inner;
    }
    expr
}

/// The literal `expr` is, if it is one.
pub(crate) fn literal<'a>(b: &'a BodyCx<'_, '_>, expr: ExprId) -> Option<&'a Literal> {
    match &b.store()[expr] {
        Expr::Literal(it) => Some(it),
        _ => None,
    }
}

/// The type type checking gave `expr`, before its adjustments (rustc's `expr_ty`).
pub(crate) fn ty<'db>(b: &BodyCx<'_, 'db>, expr: ExprId) -> Ty<'db> {
    b.infer.expr_ty(expr)
}

/// The type of `expr` after its adjustments (rustc's `expr_ty_adjusted`).
pub(crate) fn ty_adjusted<'db>(b: &BodyCx<'_, 'db>, expr: ExprId) -> Ty<'db> {
    b.infer.type_of_expr_with_adjust(expr).unwrap_or_else(|| b.infer.expr_ty(expr))
}

/// The syntax `expr` is written as, parsed or expanded.
pub(crate) fn syntax(b: &BodyCx<'_, '_>, expr: ExprId) -> Option<InFile<SyntaxNode>> {
    Some(b.cx.syntax(b.expr_node(expr)?))
}

/// rustc's span of `expr`: where it is written, with the parentheses around it, which rustc's
/// lowering gives the expression in them (`rustc_ast_lowering`, `ExprKind::Paren`).
pub(crate) fn span(b: &BodyCx<'_, '_>, expr: ExprId) -> Option<Span> {
    let node = syntax(b, expr)?;
    let mut at = node.value.clone();
    while let Some(parent) = at.parent().filter(|it| ast::ParenExpr::can_cast(it.kind())) {
        at = parent;
    }
    Some(b.cx.span(node.with_value(at.text_range())))
}

/// rustc's printing of `ty`, a type of the body, as a type of the body's owner (that of the
/// expression `near`, written in the body's source).
pub(crate) fn print_ty<'db>(b: &BodyCx<'_, 'db>, near: ExprId, ty: Ty<'db>) -> Option<String> {
    let node = syntax(b, near)?;
    let expr = ast::Expr::cast(node.value)?;
    let of = b.cx.sema.type_of_expr(&expr)?.original;
    Some(b.cx.ty(&of.derived_from_next_solver(ty)))
}

/// The trait solver over a body's types, in the typing mode of rustc's late lints
/// (`LateContext::typing_env`: `non_body_analysis` of the item).
pub(crate) struct Solver<'db> {
    pub(crate) infcx: InferCtxt<'db>,
    pub(crate) env: ParamEnv<'db>,
}

impl<'db> Solver<'db> {
    pub(crate) fn of(b: &BodyCx<'_, 'db>) -> Solver<'db> {
        let db = b.db();
        let env = db.trait_environment(b.owner.generic_def(db));
        let interner = DbInterner::new_with(db, b.owner.krate(db));
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        Solver { infcx, env }
    }

    pub(crate) fn interner(&self) -> DbInterner<'db> {
        self.infcx.interner
    }

    /// rustc's `Ty::is_sized`.
    pub(crate) fn is_sized(&self, ty: Ty<'db>) -> bool {
        self.infcx.type_is_sized_modulo_regions(self.env, ty)
    }

    /// rustc's `Ty::is_copy_modulo_regions`.
    pub(crate) fn is_copy(&self, ty: Ty<'db>) -> bool {
        self.infcx.type_is_copy_modulo_regions(self.env, ty)
    }

    /// rustc's `Ty::is_freeze`.
    pub(crate) fn is_freeze(&self, ty: Ty<'db>) -> bool {
        hir_ty::drop::is_freeze(&self.infcx, ty, self.env)
    }
}

/// rustc's `Span::find_ancestor_inside`: `span`, or the call of the macro it came from, and of
/// the macro that call came from, .., the first that is inside `outer`.
pub(crate) fn ancestor_inside(span: &Span, outer: &Span) -> Option<Span> {
    let mut at = span.clone();
    loop {
        if at.file == outer.file && outer.lo <= at.lo && at.hi <= outer.hi {
            return Some(at);
        }
        at = at.expansion?.call_site;
    }
}

/// rustc's `Span::from_expansion`: the span is in a macro's expansion.
pub(crate) fn from_expansion(span: &Span) -> bool {
    span.expansion.is_some()
}

/// rustc's `in_external_macro` of where `expr` is written: in the expansion of a macro of
/// another crate (std's `vec!`, `format_args!`), not handed to it by the caller.
pub(crate) fn in_external_macro(b: &BodyCx<'_, '_>, expr: ExprId) -> bool {
    let Some(node) = b.expr_node(expr) else { return false };
    if !from_expansion(&b.cx.span_of(node)) {
        return false;
    }
    let Some(call) = node.file_id.macro_file() else { return false };
    hir::Crate::from(call.loc(b.db()).def.krate) != b.cx.krate
}
