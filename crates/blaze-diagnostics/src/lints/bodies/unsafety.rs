//! `unused_unsafe`: an `unsafe` block with no unsafe operation of its own, or one nested in
//! another `unsafe` block that has one.
//!
//! Follows `compiler/rustc_mir_build/src/check_unsafety.rs` (`UnsafetyVisitor`:
//! `in_safety_context`, `requires_unsafe`, `visit_block`, `visit_inner_body`; `check_unsafety`:
//! the warnings sorted by span, at the head of each block) and `diagnostics.rs` (`UnusedUnsafe`,
//! `UnusedUnsafeEnclosing`) (MIT or Apache-2.0).
//!
//! What is an unsafe operation is hir's (`Semantics::get_unsafe_ops`), with the calls of the
//! functions that are unsafe only from edition 2024 (`std::env::set_var`): rustc requires
//! `unsafe` of them in every edition, and only lints a call outside a block before 2024.

use hir::{DefWithBody, InFile};
use hir_def::{
    CallableDefId, DefWithBodyId,
    expr_store::ExprOrPatSource,
    hir::{AsmOperand, Expr, ExprId, Pat, PatId, Statement, Unsafe},
};
use hir_ty::next_solver::{CallableIdWrapper, TyKind};
use rustc_hash::FxHashSet;
use rustc_type_ir::inherent::IntoKind as _;
use span::Edition;
use syntax::{AstNode, TextRange, TextSize, ast};

use super::BodyCx;
use crate::{
    lower::{Found, Phase},
    model::{Diag, Span},
};

pub(super) fn lints(b: &BodyCx<'_, '_>) -> Vec<Found> {
    if b.tainted {
        return Vec::new();
    }
    let mut visitor = Visitor { b, ops: operations(b), context: Context::Safe, warnings: Vec::new() };
    visitor.expr(b.body.root_expr());
    let mut warnings: Vec<(ExprId, Option<ExprId>)> = visitor.warnings;
    let blocks: Vec<_> = warnings.iter().map(|&(block, _)| (block, Block::of(b, block))).collect();
    let block_of = |id: ExprId| blocks.iter().find(|(it, _)| *it == id).and_then(|(_, it)| it.clone());
    warnings.sort_by_key(|&(block, _)| block_of(block).map(|it| (it.span.file.clone(), it.span.lo, it.span.hi)));
    let mut found = Vec::new();
    for (block, enclosing) in warnings {
        let Some(at) = block_of(block) else { continue };
        let mut diag = Diag::lint("unused_unsafe", "unnecessary `unsafe` block", at.head.clone());
        diag.span_label(at.head.clone(), "unnecessary `unsafe` block");
        if let Some(outer) = enclosing.and_then(|it| Block::of(b, it)) {
            diag.span_label(outer.head, "because it's nested under this `unsafe` block");
        }
        found.push(b.lint(diag, Phase::Mir, at.node));
    }
    found
}

/// rustc's `SafetyContext`, of what decides whether a block is used: the body's own context
/// (safe or an `unsafe fn`) is neither.
enum Context {
    Safe,
    /// A block lowering makes `unsafe` (`format_args!`'s): what is in it uses no block around.
    Builtin,
    Block { block: ExprId, used: bool, nested_used: Vec<ExprId> },
}

struct Visitor<'s, 'a, 'db> {
    b: &'s BodyCx<'a, 'db>,
    ops: FxHashSet<ExprOrPatSource>,
    context: Context,
    /// Each block found unused, with the used block it is nested in when that is why.
    warnings: Vec<(ExprId, Option<ExprId>)>,
}

impl Visitor<'_, '_, '_> {
    fn in_context(&mut self, context: Context, f: impl FnOnce(&mut Self)) {
        let prev = std::mem::replace(&mut self.context, context);
        f(self);
        let finished = std::mem::replace(&mut self.context, prev);
        let Context::Block { block, used, nested_used } = finished else { return };
        if !used {
            self.warnings.push((block, None));
            if let Context::Block { nested_used: outer, .. } = &mut self.context {
                outer.extend(nested_used);
            }
        } else {
            for nested in nested_used {
                self.warnings.push((nested, Some(block)));
            }
            if let Context::Block { nested_used: outer, .. } = &mut self.context {
                outer.push(block);
            }
        }
    }

    fn requires_unsafe(&mut self) {
        if let Context::Block { used, .. } = &mut self.context {
            *used = true;
        }
    }

    fn is_op(&self, source: Option<ExprOrPatSource>) -> bool {
        source.is_some_and(|it| self.ops.contains(&it))
    }

    fn expr(&mut self, expr: ExprId) {
        let b = self.b;
        let store = b.store();
        if self.is_op(b.source_map.expr_syntax(expr).ok()) || is_unsafe_call(b, expr) {
            self.requires_unsafe();
        }
        match store.expr(expr) {
            Expr::Block { unsafe_: Unsafe::Yes, .. } => {
                let context = match Block::is_explicit(b, expr) {
                    true => Context::Block { block: expr, used: false, nested_used: Vec::new() },
                    false => Context::Builtin,
                };
                self.in_context(context, |this| this.children(expr));
            }
            Expr::InlineAsm(asm) => {
                for (_, operand) in asm.operands.iter() {
                    match operand {
                        AsmOperand::Label(label) => {
                            let label = *label;
                            self.in_context(Context::Safe, |this| this.expr(label));
                        }
                        AsmOperand::In { expr, .. }
                        | AsmOperand::Out { expr: Some(expr), .. }
                        | AsmOperand::InOut { expr, .. }
                        | AsmOperand::Const(expr) => self.expr(*expr),
                        AsmOperand::SplitInOut { in_expr, out_expr, .. } => {
                            self.expr(*in_expr);
                            if let Some(it) = out_expr {
                                self.expr(*it);
                            }
                        }
                        AsmOperand::Out { expr: None, .. } | AsmOperand::Sym(_) => {}
                    }
                }
            }
            _ => self.children(expr),
        }
    }

    fn children(&mut self, expr: ExprId) {
        let store = self.b.store();
        let mut pats = Vec::new();
        match store.expr(expr) {
            Expr::Block { statements, .. } => {
                for statement in statements {
                    if let Statement::Let { pat, .. } = statement {
                        pats.push(*pat);
                    }
                }
            }
            Expr::Match { arms, .. } => pats.extend(arms.iter().map(|it| it.pat)),
            Expr::Let { pat, .. } => pats.push(pat),
            Expr::Closure { args, .. } => pats.extend(args.iter().copied()),
            Expr::Assignment { target, .. } => pats.push(target),
            _ => {}
        }
        for pat in pats {
            self.pat(pat);
        }
        let mut children = Vec::new();
        store.walk_child_exprs_without_pats(expr, |it| children.push(it));
        for child in children {
            stdx::ensure_sufficient_stack(|| self.expr(child));
        }
    }

    fn pat(&mut self, pat: PatId) {
        let b = self.b;
        if self.is_op(b.source_map.pat_syntax(pat).ok()) {
            self.requires_unsafe();
        }
        match b.store().pat(pat) {
            Pat::Expr(expr) | Pat::Lit(expr) => self.expr(expr),
            Pat::Range { start, end, .. } => {
                if let Some(it) = start {
                    self.expr(it);
                }
                if let Some(it) = end {
                    self.expr(it);
                }
            }
            _ => {
                let mut children = Vec::new();
                b.store().walk_pats_shallow(pat, |it| children.push(it));
                for child in children {
                    self.pat(child);
                }
            }
        }
    }
}

/// The unsafe operations of the body, by where they are written.
fn operations(b: &BodyCx<'_, '_>) -> FxHashSet<ExprOrPatSource> {
    let def = DefWithBody::from(b.owner);
    b.cx.sema.get_unsafe_ops(def.into())
}

/// Whether `expr` calls a function that is unsafe to call in edition 2024: hir's operations
/// leave out a call of one that is safe to call before, as a lint's and not an error's, but it
/// uses the block it is in all the same.
fn is_unsafe_call(b: &BodyCx<'_, '_>, expr: ExprId) -> bool {
    let db = b.db();
    let callee = match b.store().expr(expr) {
        Expr::Call { callee, .. } => match b.infer.expr_ty(callee).kind() {
            TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(it)), _) => it,
            _ => return false,
        },
        Expr::MethodCall { .. } => match b.infer.method_resolution(expr) {
            Some((it, _)) => it,
            None => return false,
        },
        _ => return false,
    };
    let caller = match DefWithBody::from(b.owner) {
        DefWithBody::Function(it) => Some(it),
        _ => None,
    };
    let DefWithBody::Function(callee) = DefWithBody::from(DefWithBodyId::FunctionId(callee)) else {
        return false;
    };
    callee.is_unsafe_to_call(db, caller, Edition::Edition2024)
}

/// An `unsafe` block as rustc points at it: its span from `unsafe` to its `}`, and the head of
/// that (`guess_head_span`: up to its `{`).
#[derive(Clone)]
struct Block {
    node: InFile<syntax::SyntaxNodePtr>,
    span: Span,
    head: Span,
}

impl Block {
    /// Whether `expr`, an `unsafe` block, is written so, not made by lowering.
    fn is_explicit(b: &BodyCx<'_, '_>, expr: ExprId) -> bool {
        let Some(node) = b.expr_node(expr) else { return false };
        let node = b.cx.syntax(node);
        ast::BlockExpr::cast(node.value).is_some_and(|it| it.unsafe_token().is_some())
    }

    fn of(b: &BodyCx<'_, '_>, expr: ExprId) -> Option<Block> {
        let node = b.expr_node(expr)?;
        let syntax = b.cx.syntax(node);
        let block = ast::BlockExpr::cast(syntax.value.clone())?;
        let start = block.unsafe_token()?.text_range().start();
        let range = TextRange::new(start, block.syntax().text_range().end());
        let text = block.syntax().text().to_string();
        let local = usize::from(start - block.syntax().text_range().start());
        let written = &text[local..];
        let head = written.split('{').next().unwrap_or("").trim_end();
        let head = match !head.is_empty() && !head.contains('\n') {
            true => TextRange::at(start, TextSize::of(head)),
            false => range,
        };
        Some(Block {
            node,
            span: b.cx.span(InFile::new(syntax.file_id, range)),
            head: b.cx.span(InFile::new(syntax.file_id, head)),
        })
    }
}
