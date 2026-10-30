//! When the delimiters around an expression are needed for it to parse as it does: rustc's
//! `UnusedDelimLint::is_expr_delims_necessary`, and the parser's classifications it reads, of
//! what each expression is once expanded.
//!
//! Follows rustc's `compiler/rustc_lint/src/unused.rs` (`is_expr_delims_necessary`),
//! `compiler/rustc_ast/src/util/classify.rs` (`expr_is_complete`,
//! `expr_requires_semi_to_be_stmt`, `expr_trailing_brace`, `type_trailing_braced_mac_call`) and
//! `compiler/rustc_ast/src/util/parser.rs` (`contains_exterior_struct_lit`) (MIT or
//! Apache-2.0).

use syntax::ast::{self, RangeItem as _};

use super::{Ctx, MACRO_DEPTH, expanded, expanded_ty};
use crate::cx::Cx;

/// Whether the delimiters around `inner` (expanded) are needed where it is: before the `else`
/// of a `let`-`else`, a range as a `let` scrutinee, `&raw`, a leftmost operand that would end
/// a statement, and before a block, what would run into it.
pub(super) fn delims_necessary(cx: &Cx<'_, '_>, inner: &ast::Expr, ctx: Ctx, followed_by_block: bool) -> bool {
    if ctx == Ctx::AssignedValueLetElse && (is_lazy(inner) || trailing_brace(cx, inner)) {
        return true;
    }
    if matches!(inner, ast::Expr::RangeExpr(_)) && ctx == Ctx::LetScrutineeExpr {
        return true;
    }
    // Kept around `&raw const x`: a method called on it needs them.
    if matches!(inner, ast::Expr::RefExpr(it) if it.raw_token().is_some()) {
        return true;
    }
    // `({ 0 } + 1)`: without them, `{ 0 }` is a statement.
    let mut innermost = inner.clone();
    loop {
        let next = match &innermost {
            ast::Expr::BinExpr(it) if is_binary(it) => it.lhs(),
            ast::Expr::CallExpr(it) => it.expr(),
            ast::Expr::CastExpr(it) => it.expr(),
            ast::Expr::IndexExpr(it) => it.base(),
            _ => break,
        };
        let Some(next) = next else { break };
        innermost = expanded(cx, next);
        if !requires_semi_to_be_stmt(&innermost) {
            return true;
        }
    }
    if !followed_by_block {
        return false;
    }
    // `match &(S { f }) {}`.
    let mut innermost = inner.clone();
    while let ast::Expr::RefExpr(it) = &innermost
        && let Some(e) = it.expr()
    {
        innermost = expanded(cx, e);
    }
    if contains_exterior_struct_lit(cx, &innermost) {
        return true;
    }
    // `if (() == return) {}`: what ends the expression would take the block.
    let mut innermost = inner.clone();
    loop {
        let next = match &innermost {
            ast::Expr::PrefixExpr(it) => it.expr(),
            ast::Expr::BinExpr(it) => it.rhs(),
            ast::Expr::ReturnExpr(_) | ast::Expr::YieldExpr(_) | ast::Expr::YeetExpr(_) => return true,
            // `if (break 'a i) {}`: `i {}` would be a struct literal.
            ast::Expr::BreakExpr(it) => {
                return it.expr().is_some_and(|it| {
                    let it = expanded(cx, it);
                    is_block(&it) || matches!(it, ast::Expr::PathExpr(_))
                });
            }
            ast::Expr::RangeExpr(it) if it.end().is_some() => {
                return it.end().is_some_and(|it| is_block(&expanded(cx, it)));
            }
            _ => return contains_exterior_struct_lit(cx, inner),
        };
        let Some(next) = next else { return contains_exterior_struct_lit(cx, inner) };
        innermost = expanded(cx, next);
    }
}

/// A binary operation that is not an assignment: rustc's `ExprKind::Binary`.
fn is_binary(e: &ast::BinExpr) -> bool {
    e.op_kind().is_some_and(|it| !matches!(it, ast::BinaryOp::Assignment { .. }))
}

/// `a && b` or `a || b`.
pub(super) fn is_lazy(e: &ast::Expr) -> bool {
    matches!(e, ast::Expr::BinExpr(it) if matches!(it.op_kind(), Some(ast::BinaryOp::LogicOp(_))))
}

/// `a = b`: rustc's `ExprKind::Assign`, not a compound assignment.
pub(super) fn is_assign(e: &ast::Expr) -> bool {
    matches!(e, ast::Expr::BinExpr(it) if it.op_kind() == Some(ast::BinaryOp::Assignment { op: None }))
}

/// A block, labelled or `unsafe` or not: rustc's `ExprKind::Block`, which `async`, `gen`,
/// `const` and `try` blocks are not.
pub(super) fn is_block(e: &ast::Expr) -> bool {
    matches!(e, ast::Expr::BlockExpr(it) if is_block_kind(it))
}

fn is_block_kind(block: &ast::BlockExpr) -> bool {
    block.async_token().is_none()
        && block.gen_token().is_none()
        && block.const_token().is_none()
        && block.try_block_modifier().is_none()
}

/// A block with no label that is not `unsafe`: rustc's `ExprKind::Block(_, None)` with
/// `BlockCheckMode::Default`.
pub(super) fn is_plain_block(block: &ast::BlockExpr) -> bool {
    is_block_kind(block) && block.label().is_none() && block.unsafe_token().is_none()
}

/// `expr_is_complete`: an expression that ends a statement where it ends.
fn is_complete(e: &ast::Expr) -> bool {
    match e {
        ast::Expr::IfExpr(_)
        | ast::Expr::MatchExpr(_)
        | ast::Expr::WhileExpr(_)
        | ast::Expr::LoopExpr(_)
        | ast::Expr::ForExpr(_) => true,
        // Blocks, `try` and `const` blocks; not `async` or `gen` ones.
        ast::Expr::BlockExpr(it) => it.async_token().is_none() && it.gen_token().is_none(),
        _ => false,
    }
}

/// `expr_requires_semi_to_be_stmt`.
fn requires_semi_to_be_stmt(e: &ast::Expr) -> bool {
    match e {
        ast::Expr::MacroExpr(it) => !it.macro_call().is_some_and(|it| braced(&it)),
        _ => !is_complete(e),
    }
}

/// Whether a macro is called with braces.
fn braced(call: &ast::MacroCall) -> bool {
    call.token_tree().is_some_and(|it| it.l_curly_token().is_some())
}

/// `expr_trailing_brace(..).is_some()`: the expression ends in a `}`.
fn trailing_brace(cx: &Cx<'_, '_>, e: &ast::Expr) -> bool {
    let mut e = e.clone();
    for _ in 0..MACRO_DEPTH {
        let next = match &e {
            ast::Expr::RefExpr(it) => it.expr(),
            ast::Expr::BinExpr(it) => it.rhs(),
            ast::Expr::LetExpr(it) => it.expr(),
            ast::Expr::PrefixExpr(it) => it.expr(),
            ast::Expr::BecomeExpr(it) => it.expr(),
            ast::Expr::ClosureExpr(it) => it.body(),
            ast::Expr::BreakExpr(it) => it.expr(),
            ast::Expr::RangeExpr(it) => it.end(),
            ast::Expr::ReturnExpr(it) => it.expr(),
            ast::Expr::YeetExpr(it) => it.expr(),
            ast::Expr::YieldExpr(it) => it.expr(),
            ast::Expr::BlockExpr(_)
            | ast::Expr::ForExpr(_)
            | ast::Expr::IfExpr(_)
            | ast::Expr::LoopExpr(_)
            | ast::Expr::MatchExpr(_)
            | ast::Expr::RecordExpr(_)
            | ast::Expr::WhileExpr(_) => return true,
            ast::Expr::CastExpr(it) => return it.ty().is_some_and(|it| type_trailing_braced_mac_call(cx, it)),
            ast::Expr::MacroExpr(it) => return it.macro_call().is_some_and(|it| braced(&it)),
            _ => return false,
        };
        let Some(next) = next else { return false };
        e = expanded(cx, next);
    }
    false
}

/// `type_trailing_braced_mac_call(..).is_some()`: a type that ends in a macro called with
/// braces.
fn type_trailing_braced_mac_call(cx: &Cx<'_, '_>, ty: ast::Type) -> bool {
    let mut ty = expanded_ty(cx, ty);
    for _ in 0..MACRO_DEPTH {
        let next = match &ty {
            ast::Type::MacroType(it) => return it.macro_call().is_some_and(|it| braced(&it)),
            ast::Type::PtrType(it) => it.ty(),
            ast::Type::RefType(it) => it.ty(),
            ast::Type::ForType(it) => it.ty(),
            ast::Type::FnPtrType(it) => it.ret_type().and_then(|it| it.ty()),
            ast::Type::PathType(it) => it.path().and_then(|it| path_return_type(&it)),
            ast::Type::DynTraitType(it) => it.type_bound_list().and_then(|it| last_bound_return_type(&it)),
            ast::Type::ImplTraitType(it) => it.type_bound_list().and_then(|it| last_bound_return_type(&it)),
            _ => return false,
        };
        let Some(next) = next else { return false };
        ty = expanded_ty(cx, next);
    }
    false
}

/// The return type of the last bound of a list, when it is a trait bound with one.
fn last_bound_return_type(list: &ast::TypeBoundList) -> Option<ast::Type> {
    let bound = list.bounds().last()?;
    path_return_type(&super::trait_bound_path(&bound)?)
}

/// `path_return_type`: the `-> T` of a path's last segment, as in `FnOnce(&str) -> T`.
fn path_return_type(path: &ast::Path) -> Option<ast::Type> {
    let segment = path.segment()?;
    segment.parenthesized_arg_list()?;
    segment.ret_type()?.ty()
}

/// `contains_exterior_struct_lit`: a struct literal not inside delimiters of its own, which
/// before a block would take the block as its fields.
fn contains_exterior_struct_lit(cx: &Cx<'_, '_>, e: &ast::Expr) -> bool {
    let sub = |it: Option<ast::Expr>| it.is_some_and(|it| contains_exterior_struct_lit(cx, &expanded(cx, it)));
    match e {
        ast::Expr::RecordExpr(_) => true,
        ast::Expr::BinExpr(it) => sub(it.lhs()) || sub(it.rhs()),
        ast::Expr::AwaitExpr(it) => sub(it.expr()),
        ast::Expr::PrefixExpr(it) => sub(it.expr()),
        ast::Expr::CastExpr(it) => sub(it.expr()),
        ast::Expr::FieldExpr(it) => sub(it.expr()),
        ast::Expr::IndexExpr(it) => sub(it.base()),
        ast::Expr::MethodCallExpr(it) => sub(it.receiver()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use syntax::{AstNode, Edition, SourceFile};

    use super::*;

    fn expr(text: &str) -> ast::Expr {
        let file = SourceFile::parse(&format!("fn f() {{ let _ = {text}; }}"), Edition::CURRENT).tree();
        let init = file.syntax().descendants().find_map(ast::LetStmt::cast).and_then(|it| it.initializer());
        init.unwrap()
    }

    #[test]
    fn blocks_end_a_statement_and_async_ones_do_not() {
        assert!(!requires_semi_to_be_stmt(&expr("{ 0 }")));
        assert!(!requires_semi_to_be_stmt(&expr("unsafe { 0 }")));
        assert!(!requires_semi_to_be_stmt(&expr("const { 0 }")));
        assert!(!requires_semi_to_be_stmt(&expr("if a { 0 } else { 1 }")));
        assert!(requires_semi_to_be_stmt(&expr("async { 0 }")));
        assert!(requires_semi_to_be_stmt(&expr("a + 1")));
    }

    #[test]
    fn a_block_kind_is_rustcs() {
        assert!(is_block(&expr("'a: { 0 }")));
        assert!(is_block(&expr("unsafe { 0 }")));
        assert!(!is_block(&expr("async { 0 }")));
        assert!(!is_block(&expr("const { 0 }")));
        let ast::Expr::BlockExpr(labelled) = expr("'a: { 0 }") else { panic!() };
        assert!(!is_plain_block(&labelled));
        let ast::Expr::BlockExpr(plain) = expr("{ 0 }") else { panic!() };
        assert!(is_plain_block(&plain));
    }

    #[test]
    fn lazy_and_plain_assignments_are_told_apart() {
        assert!(is_lazy(&expr("a && b")));
        assert!(is_lazy(&expr("a || b")));
        assert!(!is_lazy(&expr("a & b")));
        assert!(is_assign(&expr("a = b")));
        assert!(!is_assign(&expr("a += b")));
    }

    #[test]
    fn a_parenthesized_trait_path_has_a_return_type() {
        assert!(path_return_type(&path("FnOnce(&str) -> u8")).is_some());
        assert!(path_return_type(&path("FnOnce(&str)")).is_none());
    }

    fn path(text: &str) -> ast::Path {
        let file = SourceFile::parse(&format!("type T = dyn {text};"), Edition::CURRENT).tree();
        file.syntax().descendants().find_map(ast::Path::cast).unwrap()
    }
}
