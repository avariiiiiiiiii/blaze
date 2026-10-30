//! E0268, E0267 and E0571: a `break` or `continue` with no loop or labeled block to leave, one
//! inside a closure or a coroutine, and a `break` with a value out of a `while` or `for` loop.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/loops.rs` (`CheckLoopVisitor`: the stack of
//! contexts a body is visited with, `require_break_cx`, `report_outside_loop_error`) and
//! `compiler/rustc_hir_typeck/src/diagnostics.rs` (`OutsideLoop`, `BreakInsideClosure`,
//! `BreakInsideCoroutine`, `BreakNonLoop`) (MIT or Apache-2.0). The stack is rebuilt from the
//! syntax: from the body the `break` is in down to it, as rustc's visitor pushes it.

use hir::InFile;
use syntax::{
    AstNode, SyntaxNode, TextRange, TextSize,
    ast::{self, HasLoopBody},
};

use crate::{
    cx::{Cx, signature_range},
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// A context of rustc's loop checker.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Context {
    Normal,
    Fn,
    Loop,
    Closure(InFile<TextRange>),
    Coroutine { span: InFile<TextRange>, kind: &'static str, source: &'static str },
    /// A block with no label, by where it starts.
    UnlabeledBlock(InFile<TextSize>),
    UnlabeledIfBlock,
    LabeledBlock,
    AnonConst,
    ConstBlock,
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::BreakOutsideOfLoop) -> Vec<Found> {
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let node = cx.syntax(at);
    let span = cx.span_of(at);
    let name = if d.is_break { "break" } else { "continue" };
    let mut diag = if d.bad_value_break {
        match break_non_loop(cx, &node) {
            Some(it) => it,
            None => return Vec::new(),
        }
    } else {
        let stack = contexts(cx, &node);
        match required(&stack, d.is_break) {
            None => return Vec::new(),
            Some(Context::Closure(closure)) => {
                let mut diag =
                    Diag::error(Some("E0267"), format!("`{name}` inside of a closure"), span.clone());
                diag.span_label(span, format!("cannot `{name}` inside of a closure"));
                diag.span_label(cx.span(closure), "enclosing closure");
                diag
            }
            Some(Context::Coroutine { span: coroutine, kind, source }) => {
                let mut diag = Diag::error(
                    Some("E0267"),
                    format!("`{name}` inside `{kind}` {source}"),
                    span.clone(),
                );
                diag.span_label(span, format!("cannot `{name}` inside `{kind}` {source}"));
                diag.span_label(cx.span(coroutine), format!("enclosing `{kind}` {source}"));
                diag
            }
            Some(Context::UnlabeledBlock(block)) if same_file(&node, &block) => {
                match block_breaks(cx, &node, block) {
                    Some(it) => it,
                    None => return Vec::new(),
                }
            }
            Some(_) => outside_loop(vec![span], d.is_break),
        }
    };
    code_fact(cx, &mut diag, at);
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's `OutsideLoop`, of the `break`s or the `continue` at `spans`.
fn outside_loop(spans: Vec<crate::model::Span>, is_break: bool) -> Diag {
    let (name, or_block) = match is_break {
        true => ("break", " or labeled block"),
        false => ("continue", ""),
    };
    let mut spans = spans.into_iter();
    let first = spans.next().expect("a `break` to report");
    let mut diag =
        Diag::error(Some("E0268"), format!("`{name}` outside of a loop{or_block}"), first.clone());
    let label = format!("cannot `{name}` outside of a loop{or_block}");
    diag.span_label(first, label.clone());
    for span in spans {
        diag.primary(span.clone());
        diag.span_label(span, label.clone());
    }
    diag
}

/// rustc's one `OutsideLoop` for all the `break`s whose context is the unlabeled block at
/// `block`, with its suggestion to label it: given with the first of them, none with the
/// others.
fn block_breaks(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>, block: InFile<TextSize>) -> Option<Diag> {
    let block_node = node
        .value
        .ancestors()
        .filter_map(ast::BlockExpr::cast)
        .find(|it| it.syntax().text_range().start() == block.value)?;
    let breaks: Vec<ast::BreakExpr> = block_node
        .syntax()
        .descendants()
        .filter_map(ast::BreakExpr::cast)
        .filter(|it| {
            let other = node.with_value(it.syntax().clone());
            required(&contexts(cx, &other), true) == Some(Context::UnlabeledBlock(block))
        })
        .collect();
    let first = breaks.first()?;
    if first.syntax() != &node.value {
        return None;
    }
    let spans = breaks.iter().map(|it| cx.span(node.with_value(it.syntax().text_range()))).collect();
    let mut diag = outside_loop(spans, true);
    let mut parts = vec![(cx.span(node.with_value(TextRange::empty(block.value))), "'block: ".to_owned())];
    for it in &breaks {
        let lo = match it.break_token() {
            Some(token) => token.text_range().end(),
            None => it.syntax().text_range().start() + TextSize::of("break"),
        };
        let hi = it.lifetime().map_or(lo, |label| label.syntax().text_range().end());
        parts.push((cx.span(node.with_value(TextRange::new(lo, hi))), " 'block".to_owned()));
    }
    diag.multipart_suggestion(
        "consider labeling this block to be able to break within it",
        parts,
        Applicability::MaybeIncorrect,
    );
    Some(diag)
}

/// rustc's `BreakNonLoop`: a `break` with a value whose target is a `while` or `for` loop.
fn break_non_loop(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<Diag> {
    let break_expr = ast::BreakExpr::cast(node.value.clone())?;
    let value = break_expr.expr()?;
    let break_label = break_expr.lifetime().map(|it| it.text().to_string());
    let target = target_loop(&node.value, break_label.as_deref())?;
    let (kind, head_end, loop_label) = match &target {
        ast::Expr::WhileExpr(it) => {
            ("while", it.condition()?.syntax().text_range().end(), label_of(it.label()))
        }
        ast::Expr::ForExpr(it) => {
            ("for", it.iterable()?.syntax().text_range().end(), label_of(it.label()))
        }
        _ => return None,
    };
    let span = cx.span(node.with_value(break_expr.syntax().text_range()));
    let head = TextRange::new(target.syntax().text_range().start(), head_end);
    let mut diag = Diag::error(
        Some("E0571"),
        format!("`break` with value from a `{kind}` loop"),
        span.clone(),
    );
    diag.span_label(span.clone(), "can only break with a value inside `loop` or breakable block");
    diag.span_label(cx.span(node.with_value(head)), format!("you can't `break` with a value in a `{kind}` loop"));
    let suggestion = match &break_label {
        Some(label) => format!("break {label}"),
        None => "break".to_owned(),
    };
    diag.suggestion(
        span,
        format!("use `break` on its own without a value inside this `{kind}` loop"),
        suggestion,
        Applicability::MaybeIncorrect,
    );
    if let (Some(label), None) = (loop_label, &break_label) {
        let redundant = match &value {
            ast::Expr::PathExpr(path) => path.path().is_some_and(|path| {
                path.qualifier().is_none()
                    && path.segment().is_some_and(|it| format!("'{}", it.syntax().text()) == label)
                    && cx.sema.resolve_path(&path).is_none()
            }),
            _ => false,
        };
        if redundant {
            return None;
        }
        diag.suggestion(
            cx.span(node.with_value(value.syntax().text_range())),
            "alternatively, you might have meant to use the available loop label",
            label,
            Applicability::MaybeIncorrect,
        );
    }
    Some(diag)
}

fn label_of(label: Option<ast::Label>) -> Option<String> {
    Some(label?.lifetime()?.text().to_string())
}

/// The loop a `break` at `node` leaves: the innermost one when it has no label, else the loop
/// or labeled block with its label, within the closure or body it is in.
fn target_loop(node: &SyntaxNode, label: Option<&str>) -> Option<ast::Expr> {
    let mut child = node.clone();
    for ancestor in node.ancestors().skip(1) {
        if ast::ClosureExpr::can_cast(ancestor.kind()) || ast::Item::can_cast(ancestor.kind()) {
            return None;
        }
        let in_body = |body: Option<ast::BlockExpr>| body.is_some_and(|it| it.syntax() == &child);
        let found = match ast::Expr::cast(ancestor.clone()) {
            Some(ast::Expr::LoopExpr(it)) if in_body(it.loop_body()) => {
                Some((label_of(it.label()), false, ast::Expr::LoopExpr(it)))
            }
            Some(ast::Expr::WhileExpr(it))
                if in_body(it.loop_body())
                    || it.condition().is_some_and(|it| it.syntax() == &child) =>
            {
                Some((label_of(it.label()), false, ast::Expr::WhileExpr(it)))
            }
            Some(ast::Expr::ForExpr(it)) if in_body(it.loop_body()) => {
                Some((label_of(it.label()), false, ast::Expr::ForExpr(it)))
            }
            Some(ast::Expr::BlockExpr(it)) if it.label().is_some() => {
                Some((label_of(it.label()), true, ast::Expr::BlockExpr(it)))
            }
            _ => None,
        };
        if let Some((own, is_block, expr)) = found {
            match label {
                None if !is_block => return Some(expr),
                Some(label) if own.as_deref() == Some(label) => return Some(expr),
                _ => {}
            }
        }
        child = ancestor;
    }
    None
}

fn same_file(node: &InFile<SyntaxNode>, block: &InFile<TextSize>) -> bool {
    node.file_id == block.file_id
}

/// What `require_break_cx` finds wrong with a `break` (or `continue`) in `stack`, if anything:
/// the context it reports, an `if` block standing for the context around it.
fn required(stack: &[Context], is_break: bool) -> Option<Context> {
    let mut at = stack.len().checked_sub(1)?;
    loop {
        match &stack[at] {
            Context::LabeledBlock | Context::Loop => return None,
            Context::UnlabeledIfBlock if is_break && at > 0 => at -= 1,
            Context::UnlabeledBlock(_) if !is_break => return Some(Context::Normal),
            other => return Some(other.clone()),
        }
    }
}

/// The nodes from `node` up, through each macro expansion to its call: `node` first.
pub(crate) fn ancestors(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Vec<InFile<SyntaxNode>> {
    let mut chain = Vec::new();
    let mut at = node.clone();
    loop {
        chain.extend(at.value.ancestors().map(|it| at.with_value(it)));
        let Some(call) = at.file_id.macro_file() else { return chain };
        at = call.loc(cx.db).to_node(cx.db);
    }
}

/// The stack of contexts rustc's loop checker has at `node`.
fn contexts(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Vec<Context> {
    let chain = ancestors(cx, node);
    let (boundary, base) = chain
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(i, it)| base(it, &chain[i - 1].value).map(|base| (i, base)))
        .unwrap_or((chain.len(), Context::Normal));
    let mut stack = vec![Context::Normal, base];
    let mut contents_only = false;
    for i in (1..boundary).rev() {
        let (node, child) = (&chain[i], &chain[i - 1]);
        let skip = std::mem::take(&mut contents_only);
        if let Some(pushed) = visit(&stack, node, &child.value, skip, &mut contents_only) {
            stack.push(pushed);
        }
    }
    stack
}

/// The context a body starts in, when `node` is the owner of a body `child` is: rustc's
/// `check`, `Fn` but for an anonymous const; an `async` or `gen` function's body is the
/// coroutine it makes.
fn base(node: &InFile<SyntaxNode>, child: &SyntaxNode) -> Option<Context> {
    let value = &node.value;
    if let Some(it) = ast::Fn::cast(value.clone()) {
        if !it.body().is_some_and(|body| body.syntax() == child) {
            return None;
        }
        let kind = match (it.async_token().is_some(), it.gen_token().is_some()) {
            (true, true) => "async gen",
            (true, false) => "async",
            (false, true) => "gen",
            (false, false) => return Some(Context::Fn),
        };
        let span = node.with_value(signature_range(&it));
        return Some(Context::Coroutine { span, kind, source: "function" });
    }
    if ast::Const::can_cast(value.kind()) || ast::Static::can_cast(value.kind()) {
        return Some(Context::Fn);
    }
    // A discriminant, an array's length, a const generic argument.
    if ast::ConstArg::can_cast(value.kind()) {
        return Some(Context::AnonConst);
    }
    if let Some(it) = ast::ArrayExpr::cast(value.clone())
        && it.semicolon_token().is_some()
        && it.exprs().nth(1).is_some_and(|len| len.syntax() == child)
    {
        return Some(Context::AnonConst);
    }
    if ast::Item::can_cast(value.kind()) {
        return Some(Context::Normal);
    }
    None
}

/// What rustc's visitor pushes visiting `node` on its way to `child`, with `stack` as it is;
/// `skip` when `node` is a block whose contents are visited as an `if`'s, with nothing pushed
/// for the block itself. Sets `contents_only` when `child` is such a block.
fn visit(
    stack: &[Context],
    node: &InFile<SyntaxNode>,
    child: &SyntaxNode,
    skip: bool,
    contents_only: &mut bool,
) -> Option<Context> {
    let top = stack.last()?;
    let expr = ast::Expr::cast(node.value.clone())?;
    let is = |it: Option<ast::BlockExpr>| it.is_some_and(|it| it.syntax() == child);
    match expr {
        ast::Expr::IfExpr(it) => {
            let branch = is(it.then_branch())
                || matches!(it.else_branch(), Some(ast::ElseBranch::Block(block)) if block.syntax() == child);
            let plain = ast::BlockExpr::cast(child.clone()).is_some_and(|it| it.label().is_none());
            if branch
                && plain
                && matches!(
                    top,
                    Context::Normal | Context::AnonConst | Context::UnlabeledBlock(_) | Context::UnlabeledIfBlock
                )
            {
                *contents_only = true;
                return Some(Context::UnlabeledIfBlock);
            }
            None
        }
        ast::Expr::LoopExpr(it) => is(it.loop_body()).then_some(Context::Loop),
        ast::Expr::WhileExpr(it) => {
            let in_condition = it.condition().is_some_and(|it| it.syntax() == child);
            (is(it.loop_body()) || in_condition).then_some(Context::Loop)
        }
        ast::Expr::ForExpr(it) => is(it.loop_body()).then_some(Context::Loop),
        ast::Expr::ClosureExpr(it) => {
            if !it.body().is_some_and(|body| body.syntax() == child) {
                return None;
            }
            let start = it.syntax().text_range().start();
            let end = match it.ret_type() {
                Some(ret) => ret.syntax().text_range().end(),
                None => it.param_list()?.syntax().text_range().end(),
            };
            let span = node.with_value(TextRange::new(start, end));
            let kind = match (it.async_token().is_some(), it.gen_token().is_some()) {
                (true, true) => "async gen",
                (true, false) => "async",
                (false, true) => "gen",
                (false, false) => return Some(Context::Closure(span)),
            };
            Some(Context::Coroutine { span, kind, source: "closure" })
        }
        ast::Expr::BlockExpr(it) if !skip => block(top, node, &it),
        _ => None,
    }
}

fn block(top: &Context, node: &InFile<SyntaxNode>, it: &ast::BlockExpr) -> Option<Context> {
    if it.label().is_some() {
        return Some(Context::LabeledBlock);
    }
    let kind = match (it.async_token().is_some(), it.gen_token().is_some()) {
        (true, true) => Some("async gen"),
        (true, false) => Some("async"),
        (false, true) => Some("gen"),
        (false, false) => None,
    };
    if let Some(kind) = kind {
        let start = it.syntax().text_range().start();
        let end = it
            .stmt_list()
            .and_then(|list| list.syntax().prev_sibling_or_token())
            .into_iter()
            .flat_map(|it| std::iter::successors(Some(it), |it| it.prev_sibling_or_token()))
            .find(|it| !it.kind().is_trivia())
            .map_or(start, |it| it.text_range().end());
        let span = node.with_value(TextRange::new(start, end));
        return Some(Context::Coroutine { span, kind, source: "block" });
    }
    if it.const_token().is_some() {
        return Some(Context::ConstBlock);
    }
    if it.try_block_modifier().is_some() {
        return None;
    }
    if matches!(top, Context::Fn | Context::ConstBlock) {
        return Some(Context::Normal);
    }
    let default = it.unsafe_token().is_none();
    if default && matches!(top, Context::Normal | Context::AnonConst | Context::UnlabeledBlock(_)) {
        let start = node.with_value(it.syntax().text_range().start());
        return Some(Context::UnlabeledBlock(start));
    }
    None
}
