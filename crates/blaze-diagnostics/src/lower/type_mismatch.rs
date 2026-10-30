//! E0308 "mismatched types" in the forms rustc gives it by where the types had to agree: "expected
//! due to this" on a `let`'s type; "arguments to this function are incorrect" with the note of
//! where the function is defined; "expected `u32` because of return type"; a body with no tail
//! pointed at its return type, with "remove this semicolon to return this value"; "`match` arms
//! have incompatible types"; "`if` and `else` have incompatible types". It also gives the two
//! errors the front end reports as a mismatch of `()`: E0317, an `if` with no `else` where a
//! value is expected, and E0069, `return;` in a function that returns a value. With each:
//! rustc's "expected `u32`, found `&str`", the "expected struct `Foo<u8>` / found struct
//! `Foo<u16>`" note when the types are not simple, and the suggestions to borrow, dereference,
//! remove a borrow, convert with `.to_string()`, wrap in a variant, or add a return type.
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/infer/mod.rs`
//! (`note_type_err`, `note_error_origin`, `note_expected_found_extra`), `suggest.rs`
//! (`could_remove_semicolon`, `consider_removing_semicolon`), `compiler/rustc_middle/src/ty/
//! error.rs` (`TypeError::Sorts`, `report_maybe_different`), `compiler/rustc_middle/src/ty/
//! sty.rs` (`is_simple_ty`, `is_simple_text`), `compiler/rustc_hir_typeck/src/_match.rs`
//! (`explain_if_expr`, `maybe_get_coercion_reason`), `coercion.rs` (`report_return_mismatched_
//! types`, `ReturnNoExpression`), `fn_ctxt/checks.rs` (`check_block_with_expected`,
//! `label_fn_like`), `fn_ctxt/suggestions.rs` (`suggest_missing_return_type`,
//! `suggest_deref_or_ref`, `suggest_compatible_variants`, the conversion methods),
//! `compiler/rustc_trait_selection/src/error_reporting/traits/suggestions.rs`
//! (`ty_kind_suggestion`) and `compiler/rustc_trait_selection/src/diagnostics.rs` (`NoElse`)
//! (MIT or Apache-2.0).
//!
//! The front end does not say why the types had to agree (rustc's `ObligationCause`, requested
//! in `v2/requests/diagnostics.md`): it is found again here from the syntax around the
//! expression, which the front end reports at the innermost tail of a block as rustc does.
//!
//! Cost: nothing until type checking reports the mismatch. Then O(the syntax from the
//! expression up to the item, and a `match`'s earlier arms, each typed once): suggestions
//! compare types as interned ids, never printed.

use base_db::{CrateOrigin, LangCrateOrigin};
use hir::{AsAssocItem as _, InFile};
use syntax::{
    AstNode, SyntaxNode, TextRange, TextSize,
    ast::{self, HasArgList as _, HasName as _},
};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag, MultiSpan, Span, SuggestionStyle},
    print,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TypeMismatch<'_>) -> Vec<Found> {
    let at = d.expr_or_pat.map(|it| it.syntax_node_ptr());
    let node = cx.syntax(at);
    let expr = ast::Expr::cast(node.value.clone());
    let mut diag = if let Some(ast::Expr::IfExpr(it)) = &expr
        && it.else_branch().is_none()
    {
        no_else(cx, d, &node, it)
    } else if let Some(ast::Expr::ReturnExpr(it)) = &expr
        && it.expr().is_none()
    {
        return_no_value(cx, d, &node, it)
    } else if let Some(ast::Expr::BlockExpr(block)) = &expr
        && block.stmt_list().is_some_and(|it| it.tail_expr().is_none())
        && let Some(item) = fn_of_body(block)
        && let Some(ret) = item.ret_type().and_then(|it| it.ty())
    {
        no_tail(cx, d, &node, block, &item, &ret)
    } else {
        mismatch(cx, d, &node)
    };
    code_fact(cx, &mut diag, at);
    vec![Found::new(diag, Phase::Bodies)]
}

/// Where the expression's type was checked against what it must be: rustc's cause codes, as
/// the syntax shows them.
enum Context {
    /// An arm after the first that does not diverge: `MatchExpressionArm`.
    /// `prior` is the arms before it that do not diverge, as `block_span` gives them.
    MatchArm { match_expr: ast::MatchExpr, prior: Vec<TextRange> },
    /// The `else` of an `if`: `IfExpression`.
    Else { if_expr: ast::IfExpr },
    /// The initializer of a `let` with a type.
    Let { ty: ast::Type },
    /// An argument of a call, its index among the written arguments.
    Arg { call: Call, index: usize },
    /// The value a function returns: its body's tail, or `return`'s value.
    Return { item: ast::Fn },
    Plain,
}

enum Call {
    Call(ast::CallExpr),
    Method(ast::MethodCallExpr),
}

fn mismatch(cx: &Cx<'_, '_>, d: &hir::TypeMismatch<'_>, node: &InFile<SyntaxNode>) -> Diag {
    let span = cx.span(node.as_ref().map(|it| it.text_range()));
    let outer = outer_of(&node.value);
    let context = context(cx, &outer);
    let message = match &context {
        Context::MatchArm { .. } => "`match` arms have incompatible types",
        Context::Else { .. } => "`if` and `else` have incompatible types",
        _ => "mismatched types",
    };
    let mut diag = Diag::error(Some("E0308"), message, span.clone());
    diag.span_label(span.clone(), sorts(cx, &d.expected, &d.actual));
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    match &context {
        Context::MatchArm { match_expr, prior } => {
            let t = cx.ty(&d.expected);
            let mut multiline = is_multiline(cx, &span);
            if prior.len() <= 4 {
                for &range in prior {
                    let at = in_file(range);
                    multiline |= is_multiline(cx, &at);
                    diag.span_label(at, format!("this is found to be of type `{t}`"));
                }
            } else if let Some(&range) = prior.last() {
                let at = in_file(range);
                multiline |= is_multiline(cx, &at);
                diag.span_label(at, format!("this and all prior arms are found to be of type `{t}`"));
            }
            let whole = in_file(match_expr.syntax().text_range());
            let outer = match match_expr.expr() {
                Some(scrutinee) if multiline || !is_multiline(cx, &whole) => {
                    whole.shrink_to_lo().to(&in_file(scrutinee.syntax().text_range()))
                }
                _ => whole,
            };
            diag.span_label(outer, "`match` arms have incompatible types");
        }
        Context::Else { if_expr } => {
            let whole = in_file(if_expr.syntax().text_range());
            if let Some(ast::ElseBranch::IfExpr(inner)) = if_expr.else_branch()
                && inner.else_branch().is_none()
                && d.actual.is_unit()
            {
                diag.note("`if` expressions without `else` evaluate to `()`");
                diag.note("consider adding an `else` block that evaluates to the expected type");
            }
            let then = if_expr.then_branch().map(|it| in_file(block_span(&ast::Expr::BlockExpr(it))));
            let else_ = if_expr.else_branch().map(|it| match it {
                ast::ElseBranch::Block(it) => in_file(block_span(&ast::Expr::BlockExpr(it))),
                ast::ElseBranch::IfExpr(it) => in_file(it.syntax().text_range()),
            });
            if let Some(then) = &then {
                diag.span_label(then.clone(), "expected because of this");
            }
            if is_multiline(cx, &whole) {
                let ends_with_branch = [&then, &else_].iter().any(|it| it.as_ref().is_some_and(|it| it.hi == whole.hi));
                let outer = match if_expr.condition() {
                    Some(cond) if ends_with_branch => whole.shrink_to_lo().to(&in_file(cond.syntax().text_range())),
                    _ => whole,
                };
                diag.span_label(outer, "`if` and `else` have incompatible types");
            }
        }
        Context::Let { ty } => {
            diag.span_label(in_file(ty.syntax().text_range()), "expected due to this");
        }
        Context::Arg { call, index } => arg_context(cx, node, call, *index, &mut diag),
        Context::Return { item } => match item.ret_type().and_then(|it| it.ty()) {
            Some(ret) => {
                diag.span_label(
                    in_file(ret.syntax().text_range()),
                    format!("expected `{}` because of return type", cx.ty(&d.expected)),
                );
            }
            None if d.expected.is_unit() => missing_return_type(cx, node, item, &d.actual, &mut diag),
            None => {}
        },
        Context::Plain => {}
    }
    note_expected_found(cx, &d.expected, &d.actual, &mut diag);
    if let Some(expr) = ast::Expr::cast(node.value.clone()) {
        suggest(cx, &node.with_value(expr), &d.expected, &d.actual, &mut diag);
    }
    diag
}

/// rustc's `TypeError::Sorts`: "expected `u32`, found `&str`", "found a different" when the
/// two read the same.
fn sorts(cx: &Cx<'_, '_>, expected: &hir::Type<'_>, found: &hir::Type<'_>) -> String {
    let (e, f) = (print::sort_string(cx, expected), print::sort_string(cx, found));
    if e == f {
        format!("expected {e}, found a different {f}")
    } else {
        format!("expected {e}, found {f}")
    }
}

/// rustc's `note_expected_found`: the two types in full, aligned, unless both are simple.
fn note_expected_found<'db>(
    cx: &Cx<'_, 'db>,
    expected: &hir::Type<'db>,
    found: &hir::Type<'db>,
    diag: &mut Diag,
) {
    if is_simple_text(cx, expected) && is_simple_text(cx, found) {
        return;
    }
    let e = format!("expected {}", print::prefix_string(cx, expected));
    let f = format!("found {}", print::prefix_string(cx, found));
    let width = e.len().max(f.len());
    diag.note(format!(
        "{:>width$} `{}`\n{:>width$} `{}`",
        e,
        cx.ty(expected),
        f,
        cx.ty(found),
    ));
}

/// rustc's `is_simple_ty`.
fn is_simple_ty<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>) -> bool {
    if ty.as_builtin().is_some() || ty.is_unit() {
        return true;
    }
    let inner = match (ty.as_reference(), ty.as_slice()) {
        (Some((inner, _)), _) | (None, Some(inner)) => inner,
        (None, None) => match ty.as_array(cx.db) {
            Some((inner, _)) => inner,
            None => return false,
        },
    };
    is_simple_ty(cx, &inner.strip_references())
}

/// rustc's `is_simple_text`: a simple type, an ADT with no type arguments, or a reference to
/// one.
fn is_simple_text<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>) -> bool {
    if ty.as_adt().is_some() {
        return ty.type_arguments().next().is_none();
    }
    if let Some((inner, _)) = ty.as_reference() {
        return is_simple_text(cx, &inner);
    }
    is_simple_ty(cx, ty)
}

/// The expression `node` is the value of, through the blocks it is the tail of.
fn outer_of(node: &SyntaxNode) -> SyntaxNode {
    let mut node = node.clone();
    loop {
        let Some(list) = node.parent().and_then(ast::StmtList::cast) else { return node };
        if list.tail_expr().is_none_or(|it| it.syntax() != &node) {
            return node;
        }
        let Some(block) = list.syntax().parent().and_then(ast::BlockExpr::cast) else { return node };
        if block.async_token().is_some()
            || block.const_token().is_some()
            || block.try_block_modifier().is_some()
            || block.gen_token().is_some()
        {
            return node;
        }
        node = block.syntax().clone();
    }
}

fn context(cx: &Cx<'_, '_>, outer: &SyntaxNode) -> Context {
    let Some(parent) = outer.parent() else { return Context::Plain };
    if let Some(arm) = ast::MatchArm::cast(parent.clone())
        && arm.expr().is_some_and(|it| it.syntax() == outer)
        && let Some(match_expr) = arm.syntax().parent().and_then(|it| it.parent()).and_then(ast::MatchExpr::cast)
    {
        let prior: Vec<TextRange> = match_expr
            .match_arm_list()
            .into_iter()
            .flat_map(|it| it.arms())
            .take_while(|it| it != &arm)
            .filter_map(|it| it.expr())
            .filter(|expr| !cx.sema.type_of_expr(expr).is_some_and(|it| it.original.is_never()))
            .map(|expr| block_span(&expr))
            .collect();
        return if prior.is_empty() { Context::Plain } else { Context::MatchArm { match_expr, prior } };
    }
    if let Some(if_expr) = ast::IfExpr::cast(parent.clone()) {
        let is_else = match if_expr.else_branch() {
            Some(ast::ElseBranch::Block(it)) => it.syntax() == outer,
            Some(ast::ElseBranch::IfExpr(it)) => it.syntax() == outer,
            None => false,
        };
        return if is_else { Context::Else { if_expr } } else { Context::Plain };
    }
    if let Some(stmt) = ast::LetStmt::cast(parent.clone())
        && stmt.initializer().is_some_and(|it| it.syntax() == outer)
        && let Some(ty) = stmt.ty()
    {
        return Context::Let { ty };
    }
    if let Some(list) = ast::ArgList::cast(parent.clone())
        && let Some(index) = list.args().position(|it| it.syntax() == outer)
    {
        let call = match list.syntax().parent() {
            Some(it) if ast::CallExpr::can_cast(it.kind()) => ast::CallExpr::cast(it).map(Call::Call),
            Some(it) => ast::MethodCallExpr::cast(it).map(Call::Method),
            None => None,
        };
        return match call {
            Some(call) => Context::Arg { call, index },
            None => Context::Plain,
        };
    }
    if let Some(ret) = ast::ReturnExpr::cast(parent.clone())
        && ret.expr().is_some_and(|it| it.syntax() == outer)
    {
        return match enclosing_fn(ret.syntax()) {
            Some(item) => Context::Return { item },
            None => Context::Plain,
        };
    }
    if let Some(block) = ast::BlockExpr::cast(outer.clone())
        && let Some(item) = fn_of_body(&block)
    {
        return Context::Return { item };
    }
    Context::Plain
}

/// The function `block` is the body of.
fn fn_of_body(block: &ast::BlockExpr) -> Option<ast::Fn> {
    let item = block.syntax().parent().and_then(ast::Fn::cast)?;
    (item.body().as_ref() == Some(block)).then_some(item)
}

/// The function a `return` returns from: the nearest, unless a closure or an async block is
/// nearer.
fn enclosing_fn(node: &SyntaxNode) -> Option<ast::Fn> {
    for it in node.ancestors().skip(1) {
        if ast::ClosureExpr::can_cast(it.kind()) {
            return None;
        }
        if let Some(block) = ast::BlockExpr::cast(it.clone())
            && block.async_token().is_some()
        {
            return None;
        }
        if let Some(item) = ast::Fn::cast(it.clone()) {
            return Some(item);
        }
        if ast::Item::can_cast(it.kind()) {
            return None;
        }
    }
    None
}

/// rustc's `find_block_span`: of a block, its tail, else its last statement, else itself.
fn block_span(expr: &ast::Expr) -> TextRange {
    let ast::Expr::BlockExpr(block) = expr else { return expr.syntax().text_range() };
    let Some(list) = block.stmt_list() else { return expr.syntax().text_range() };
    if let Some(tail) = list.tail_expr() {
        return tail.syntax().text_range();
    }
    match list.statements().last() {
        Some(stmt) => stmt.syntax().text_range(),
        None => expr.syntax().text_range(),
    }
}

fn is_multiline(cx: &Cx<'_, '_>, span: &Span) -> bool {
    cx.snippet(span).is_some_and(|it| it.contains('\n'))
}

/// An argument that does not match its parameter: rustc's `report_arg_errors` with one error,
/// "arguments to this function are incorrect" on the callee and the note of where it is
/// defined, the parameter labelled.
fn arg_context(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>, call: &Call, index: usize, diag: &mut Diag) {
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    let (callee_range, callable) = match call {
        Call::Call(call) => {
            let Some(callee) = call.expr() else { return };
            (callee.syntax().text_range(), cx.sema.resolve_expr_as_callable(&callee))
        }
        Call::Method(call) => {
            let Some(name) = call.name_ref() else { return };
            let callable = cx.sema.resolve_method_call_as_callable(call);
            (name.syntax().text_range(), callable)
        }
    };
    let Some(callable) = callable else { return };
    let (call_name, def_descr) = match callable.kind() {
        hir::CallableKind::Function(f) => {
            let descr = if f.has_self_param(cx.db) {
                "method"
            } else if f.as_assoc_item(cx.db).is_some() {
                "associated function"
            } else {
                "function"
            };
            (if matches!(call, Call::Method(_)) { "method" } else { "function" }, descr)
        }
        hir::CallableKind::TupleStruct(_) => ("struct", "tuple struct"),
        hir::CallableKind::TupleEnumVariant(_) => ("enum variant", "tuple variant"),
        _ => return,
    };
    diag.span_label(in_file(callee_range), format!("arguments to this {call_name} are incorrect"));
    let Some((name, params)) = def_params(cx, &callable) else { return };
    let mut at = MultiSpan::of(name);
    // A method called by its path takes its receiver as the first argument.
    let index = match callable.kind() {
        hir::CallableKind::Function(f) if f.has_self_param(cx.db) && matches!(call, Call::Call(_)) => {
            index.checked_sub(1).map_or(Some(None), |it| Some(Some(it)))
        }
        _ => Some(Some(index)),
    };
    match index {
        Some(Some(index)) => {
            if let Some(param) = params.params.get(index) {
                at.labels.push((param.clone(), String::new()));
            }
        }
        Some(None) => {
            if let Some(param) = &params.self_param {
                at.labels.push((param.clone(), String::new()));
            }
        }
        None => {}
    }
    diag.span_note(at, format!("{def_descr} defined here"));
}

struct Params {
    self_param: Option<Span>,
    params: Vec<Span>,
}

/// The name of what is called, and its parameters (or a tuple struct's or variant's fields).
fn def_params(cx: &Cx<'_, '_>, callable: &hir::Callable<'_>) -> Option<(Span, Params)> {
    match callable.kind() {
        hir::CallableKind::Function(f) => {
            let src = cx.sema.source(f)?;
            let name = src.value.name()?;
            let list = src.value.param_list()?;
            let span = |range: TextRange| cx.span(src.with_value(range));
            Some((
                span(name.syntax().text_range()),
                Params {
                    self_param: list.self_param().map(|it| span(it.syntax().text_range())),
                    params: list.params().map(|it| span(it.syntax().text_range())).collect(),
                },
            ))
        }
        hir::CallableKind::TupleStruct(s) => {
            let src = cx.sema.source(s)?;
            let name = src.value.name()?;
            let fields = match src.value.field_list()? {
                ast::FieldList::TupleFieldList(it) => it.fields().map(|f| f.syntax().text_range()).collect(),
                ast::FieldList::RecordFieldList(_) => Vec::new(),
            };
            let span = |range: TextRange| cx.span(src.with_value(range));
            Some((
                span(name.syntax().text_range()),
                Params { self_param: None, params: fields.into_iter().map(span).collect() },
            ))
        }
        hir::CallableKind::TupleEnumVariant(v) => {
            let src = cx.sema.source(v)?;
            let name = src.value.name()?;
            let fields = match src.value.field_list()? {
                ast::FieldList::TupleFieldList(it) => it.fields().map(|f| f.syntax().text_range()).collect(),
                ast::FieldList::RecordFieldList(_) => Vec::new(),
            };
            let span = |range: TextRange| cx.span(src.with_value(range));
            Some((
                span(name.syntax().text_range()),
                Params { self_param: None, params: fields.into_iter().map(span).collect() },
            ))
        }
        _ => None,
    }
}

/// rustc's `suggest_missing_return_type` of a function with no return type whose tail is a
/// value: "try adding a return type", or of `main` and trait methods the label "expected `()`
/// because of default return type".
fn missing_return_type(
    cx: &Cx<'_, '_>,
    node: &InFile<SyntaxNode>,
    item: &ast::Fn,
    found: &hir::Type<'_>,
    diag: &mut Diag,
) {
    let Some(params) = item.param_list() else { return };
    let at = cx.span(node.with_value(TextRange::empty(params.syntax().text_range().end())));
    let is_main = item.name().is_some_and(|it| it.text() == "main")
        && item.syntax().parent().is_some_and(|it| ast::SourceFile::can_cast(it.kind()));
    let in_trait_impl = item
        .syntax()
        .ancestors()
        .find_map(ast::Impl::cast)
        .is_some_and(|it| it.trait_().is_some());
    if is_main || in_trait_impl {
        diag.span_label(at, "expected `()` because of default return type");
    } else if found.contains_unknown() || found.is_closure() || found.as_callable(cx.db).is_some() {
        diag.suggestion(at, "a return type might be missing here", " -> _", Applicability::HasPlaceholders);
    } else {
        diag.suggestion(
            at,
            "try adding a return type",
            format!(" -> {}", cx.ty(found)),
            Applicability::MachineApplicable,
        );
    }
}

/// A function body with no tail where a value is to be returned: rustc points at the return
/// type, labels the function's name, and suggests removing the last statement's semicolon when
/// its value is of the type.
fn no_tail(
    cx: &Cx<'_, '_>,
    d: &hir::TypeMismatch<'_>,
    node: &InFile<SyntaxNode>,
    block: &ast::BlockExpr,
    item: &ast::Fn,
    ret: &ast::Type,
) -> Diag {
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    let span = in_file(ret.syntax().text_range());
    let mut diag = Diag::error(Some("E0308"), "mismatched types", span.clone());
    diag.span_label(span, sorts(cx, &d.expected, &d.actual));
    if let Some(name) = item.name() {
        diag.span_label(
            in_file(name.syntax().text_range()),
            "implicitly returns `()` as its body has no tail or `return` expression",
        );
    }
    if let Some(ast::Stmt::ExprStmt(stmt)) = block.stmt_list().and_then(|it| it.statements().last())
        && let Some(semi) = stmt.semicolon_token()
        && let Some(expr) = stmt.expr()
        && let Some(ty) = cx.sema.type_of_expr(&expr)
        && ty.original.could_coerce_to(cx.db, &d.expected)
    {
        diag.suggestion_with_style(
            vec![(in_file(semi.text_range()), String::new())],
            "remove this semicolon to return this value",
            Applicability::MachineApplicable,
            SuggestionStyle::HideCodeInline,
        );
    }
    note_expected_found(cx, &d.expected, &d.actual, &mut diag);
    diag
}

/// E0317: an `if` with no `else` where a value of another type than `()` is expected. The
/// front end reports `()` as what was expected, as rustc's `label_unit_as_expected` does.
fn no_else(cx: &Cx<'_, '_>, d: &hir::TypeMismatch<'_>, node: &InFile<SyntaxNode>, if_expr: &ast::IfExpr) -> Diag {
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    let span = in_file(if_expr.syntax().text_range());
    let (expected, found) = (&d.actual, &d.expected);
    let mut diag = Diag::error(Some("E0317"), "`if` may be missing an `else` clause", span.clone());
    diag.span_label(span, sorts(cx, expected, found));
    match coercion_reason(cx, node, if_expr.syntax(), expected) {
        Some((at, label)) => {
            diag.span_label(at, label);
        }
        None => {
            if let Some(tail) = if_expr.then_branch().and_then(|it| it.stmt_list()).and_then(|it| it.tail_expr()) {
                diag.span_label(in_file(tail.syntax().text_range()), "found here");
            }
        }
    }
    diag.note("`if` expressions without `else` evaluate to `()`");
    diag.help("consider adding an `else` block that evaluates to the expected type");
    diag
}

/// rustc's `maybe_get_coercion_reason`: the return type of the function `node` is the body's
/// value of, or the `let` it initializes with a type.
fn coercion_reason(
    cx: &Cx<'_, '_>,
    node: &InFile<SyntaxNode>,
    expr: &SyntaxNode,
    expected: &hir::Type<'_>,
) -> Option<(Span, String)> {
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    let outer = outer_of(expr);
    if let Some(block) = ast::BlockExpr::cast(outer.clone())
        && let Some(item) = fn_of_body(&block)
        && let Some(ret) = item.ret_type().and_then(|it| it.ty())
    {
        return Some((
            in_file(ret.syntax().text_range()),
            format!("expected `{}` because of this return type", cx.ty(expected)),
        ));
    }
    let stmt = outer.parent().and_then(ast::LetStmt::cast)?;
    stmt.ty()?;
    let pat = stmt.pat()?;
    Some((in_file(pat.syntax().text_range()), "expected because of this assignment".to_owned()))
}

/// E0069: `return;` in a function whose return type is not `()`.
fn return_no_value(
    cx: &Cx<'_, '_>,
    d: &hir::TypeMismatch<'_>,
    node: &InFile<SyntaxNode>,
    ret: &ast::ReturnExpr,
) -> Diag {
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    let span = in_file(ret.syntax().text_range());
    let expected = &d.actual;
    let mut diag =
        Diag::error(Some("E0069"), "`return;` in a function whose return type is not `()`", span.clone());
    if let Some(item) = enclosing_fn(ret.syntax())
        && let Some(ty) = item.ret_type().and_then(|it| it.ty())
    {
        diag.span_label(
            in_file(ty.syntax().text_range()),
            format!("expected `{}` because of this return type", cx.ty(expected)),
        );
    }
    if let Some(value) = value_of(cx, expected) {
        diag.suggestion_verbose(
            span.shrink_to_hi(),
            "give the `return` a value of the expected type",
            format!(" {value}"),
            Applicability::HasPlaceholders,
        );
    }
    diag.span_label(span, "return type is not `()`");
    diag
}

/// rustc's `ty_kind_suggestion`: a value of the type to write.
fn value_of(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<String> {
    if ty.is_never() || ty.contains_unknown() {
        return None;
    }
    if ty.is_bool() {
        return Some("false".to_owned());
    }
    if ty.is_char() {
        return Some("'x'".to_owned());
    }
    if ty.is_int_or_uint() {
        return Some("42".to_owned());
    }
    if ty.is_float() {
        return Some("3.14159".to_owned());
    }
    if ty.is_slice() {
        return Some("[]".to_owned());
    }
    if let Some((inner, m)) = ty.as_reference() {
        if inner.is_str() && m == hir::Mutability::Shared {
            return Some("\"\"".to_owned());
        }
        let prefix = if m == hir::Mutability::Mut { "&mut " } else { "&" };
        return value_of(cx, &inner).map(|it| format!("{prefix}{it}"));
    }
    if ty.is_unit() {
        return Some("()".to_owned());
    }
    if let Some(adt) = ty.as_adt() {
        let name = adt.name(cx.db);
        let std = matches!(
            adt.module(cx.db).krate(cx.db).origin(cx.db),
            CrateOrigin::Lang(_)
        );
        let mut args = ty.type_arguments();
        return match (std, name.as_str()) {
            (true, "Vec") => Some("vec![]".to_owned()),
            (true, "String") => Some("String::new()".to_owned()),
            (true, "Box") => args.next().and_then(|it| value_of(cx, &it)).map(|it| format!("Box::new({it})")),
            (true, "Option") => Some("None".to_owned()),
            (true, "Result") => args.next().and_then(|it| value_of(cx, &it)).map(|it| format!("Ok({it})")),
            _ => None,
        };
    }
    None
}

/// rustc's suggestions for an expression of the wrong type: borrow it, dereference it, remove
/// its borrow, convert it with `.to_string()`, or wrap it in the variant of the expected enum
/// that holds its type.
fn suggest<'t>(
    cx: &Cx<'_, '_>,
    expr: &InFile<ast::Expr>,
    expected: &hir::Type<'t>,
    found: &hir::Type<'t>,
    diag: &mut Diag,
) {
    let span = cx.span(expr.as_ref().map(|it| it.syntax().text_range()));
    if expr.file_id.is_macro() {
        return;
    }
    if let ast::Expr::Literal(lit) = &expr.value
        && let Some(suffix) = numeric_suffix(lit)
        && (expected.is_int_or_uint() && found.is_int_or_uint() || expected.is_float() && found.is_float())
    {
        let token = lit.token().text_range();
        let at = TextRange::new(token.end() - suffix, token.end());
        diag.suggestion_verbose(
            cx.span(expr.with_value(at)),
            format!(
                "change the type of the numeric literal from `{}` to `{}`",
                cx.ty(found),
                cx.ty(expected)
            ),
            cx.ty(expected),
            Applicability::MachineApplicable,
        );
        return;
    }
    if let Some((inner, m)) = expected.as_reference()
        && inner == *found
    {
        let (prefix, how) = if m == hir::Mutability::Mut { ("&mut ", "mutably ") } else { ("&", "") };
        let code = if needs_parens(&expr.value) {
            vec![(span.shrink_to_lo(), format!("{prefix}(")), (span.shrink_to_hi(), ")".to_owned())]
        } else {
            vec![(span.shrink_to_lo(), prefix.to_owned())]
        };
        diag.suggestion_with_style(
            code,
            format!("consider {how}borrowing here"),
            Applicability::MachineApplicable,
            SuggestionStyle::ShowCode,
        );
        return;
    }
    if let Some((inner, _)) = found.as_reference()
        && inner == *expected
    {
        if let ast::Expr::RefExpr(r) = &expr.value
            && let Some(operand) = r.expr()
        {
            let end = operand.syntax().text_range().start();
            let start = r.syntax().text_range().start();
            diag.suggestion_with_style(
                vec![(cx.span(expr.with_value(TextRange::new(start, end))), String::new())],
                "consider removing the borrow",
                Applicability::MachineApplicable,
                SuggestionStyle::ShowAlways,
            );
            return;
        }
        if inner.is_copy(cx.db) {
            diag.suggestion_with_style(
                vec![(span.shrink_to_lo(), "*".to_owned())],
                "consider dereferencing the borrow",
                Applicability::MachineApplicable,
                SuggestionStyle::ShowCode,
            );
            return;
        }
    }
    if is_string(cx, expected)
        && let Some((inner, _)) = found.as_reference()
        && inner.is_str()
    {
        diag.suggestion_with_style(
            vec![(span.shrink_to_hi(), ".to_string()".to_owned())],
            "try using a conversion method",
            Applicability::MaybeIncorrect,
            SuggestionStyle::ShowCode,
        );
        return;
    }
    wrap_in_variant(cx, &span, expected, found, diag);
}

/// rustc's `suggest_compatible_variants` of one variant: "try wrapping the expression in
/// `Some`".
fn wrap_in_variant<'t>(
    cx: &Cx<'_, '_>,
    span: &Span,
    expected: &hir::Type<'t>,
    found: &hir::Type<'t>,
    diag: &mut Diag,
) {
    let Some(hir::Adt::Enum(e)) = expected.as_adt() else { return };
    let args: Vec<hir::Type<'_>> = expected.type_arguments().collect();
    let mut compatible = e.variants(cx.db).into_iter().filter(|v| {
        if matches!(v.kind(cx.db), hir::StructKind::Tuple)
            && let [field] = v.fields(cx.db).as_slice()
        {
            field.ty(cx.db).instantiate_type_params(args.iter()) == *found
        } else {
            false
        }
    });
    let (Some(variant), None) = (compatible.next(), compatible.next()) else { return };
    let enum_name = e.name(cx.db);
    let in_prelude = matches!(enum_name.as_str(), "Option" | "Result")
        && matches!(e.module(cx.db).krate(cx.db).origin(cx.db), CrateOrigin::Lang(_));
    let variant_name = variant.name(cx.db);
    let path = if in_prelude {
        variant_name.as_str().to_owned()
    } else {
        format!("{}::{}", enum_name.as_str(), variant_name.as_str())
    };
    diag.multipart_suggestion(
        format!("try wrapping the expression in `{path}`"),
        vec![(span.shrink_to_lo(), format!("{path}(")), (span.shrink_to_hi(), ")".to_owned())],
        Applicability::MaybeIncorrect,
    );
}

/// The length of a numeric literal's type suffix (`f64` of `1f64`).
fn numeric_suffix(lit: &ast::Literal) -> Option<TextSize> {
    let suffix = match lit.kind() {
        ast::LiteralKind::IntNumber(it) => it.suffix().map(str::len),
        ast::LiteralKind::FloatNumber(it) => it.suffix().map(str::len),
        _ => None,
    }?;
    TextSize::try_from(suffix).ok()
}

fn is_string(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    match ty.as_adt() {
        Some(hir::Adt::Struct(it)) => {
            it.name(cx.db).as_str() == "String"
                && it.module(cx.db).krate(cx.db).origin(cx.db)
                    == CrateOrigin::Lang(LangCrateOrigin::Alloc)
        }
        _ => false,
    }
}

/// Whether `&` before the expression would bind tighter than it: rustc's `needs_parens` by
/// precedence, of the expressions that bind looser than a prefix operator.
fn needs_parens(expr: &ast::Expr) -> bool {
    matches!(
        expr,
        ast::Expr::BinExpr(_)
            | ast::Expr::CastExpr(_)
            | ast::Expr::RangeExpr(_)
            | ast::Expr::ClosureExpr(_)
            | ast::Expr::ReturnExpr(_)
            | ast::Expr::BreakExpr(_)
            | ast::Expr::YieldExpr(_)
    )
}