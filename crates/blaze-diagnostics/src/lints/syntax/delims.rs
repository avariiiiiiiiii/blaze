//! rustc's `unused_parens`, `unused_braces` and `unused_import_braces`: parentheses and braces
//! around an expression, a pattern, a type or an import that the program reads the same without.
//!
//! rustc's early lint pass sees the expanded AST: a macro call as what it expands to
//! ([`expanded`]), and what a statement macro expands to as statements of the block it is in.
//!
//! Follows rustc's `compiler/rustc_lint/src/unused.rs` (`UnusedDelimLint`, `UnusedParens`,
//! `UnusedBraces`, `UnusedImportBraces`) and `compiler/rustc_lint/src/lints.rs` (`UnusedDelim`,
//! `UnusedDelimSuggestion`, `UnusedImportBracesDiag`) (MIT or Apache-2.0).

mod precedence;

use hir::{HirFileId, InFile};
use span::{Edition, SyntaxContext};
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, TextSize,
    ast::{self, HasArgList as _, HasLoopBody as _, HasName as _},
};

use super::{Emitted, Pass, Walk};
use crate::{
    cx::Cx,
    lower::Phase,
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// How many macro calls that expand to a macro call are followed: rustc's default
/// `recursion_limit`.
const MACRO_DEPTH: usize = 128;

pub(crate) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Emitted> {
    let mut check = Check { cx, walk, emitted: Vec::new() };
    for tree in &walk.trees {
        for node in &tree.nodes {
            check.node(tree.file, node);
        }
    }
    check.emitted
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lint {
    Parens,
    Braces,
}

impl Lint {
    fn name(self) -> &'static str {
        match self {
            Lint::Parens => "unused_parens",
            Lint::Braces => "unused_braces",
        }
    }

    fn delims(self) -> &'static str {
        match self {
            Lint::Parens => "parentheses",
            Lint::Braces => "braces",
        }
    }

    /// rustc's `LINT_EXPR_IN_PATTERN_MATCHING_CTX`: braces make a copy a `ref` pattern then
    /// borrows (`let ref b = { a };`), so they are not linted where a pattern matches a value.
    fn in_pattern_matching(self) -> bool {
        self == Lint::Parens
    }
}

/// Where a delimited value is: rustc's `UnusedDelimsCtx`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Ctx {
    FunctionArg,
    MethodArg,
    AssignedValue,
    AssignedValueLetElse,
    IfCond,
    WhileCond,
    ForIterExpr,
    MatchScrutineeExpr,
    ReturnValue,
    BlockRetValue,
    BreakValue,
    LetScrutineeExpr,
    ArrayLenExpr,
    AnonConst,
    MatchArmExpr,
    IndexExpr,
    ClosureBody,
}

impl Ctx {
    fn describe(self) -> &'static str {
        match self {
            Ctx::FunctionArg => "function argument",
            Ctx::MethodArg => "method argument",
            Ctx::AssignedValue | Ctx::AssignedValueLetElse => "assigned value",
            Ctx::IfCond => "`if` condition",
            Ctx::WhileCond => "`while` condition",
            Ctx::ForIterExpr => "`for` iterator expression",
            Ctx::MatchScrutineeExpr => "`match` scrutinee expression",
            Ctx::ReturnValue => "`return` value",
            Ctx::BlockRetValue => "block return value",
            Ctx::BreakValue => "`break` value",
            Ctx::LetScrutineeExpr => "`let` scrutinee expression",
            Ctx::ArrayLenExpr | Ctx::AnonConst => "const expression",
            Ctx::MatchArmExpr => "match arm expression",
            Ctx::IndexExpr => "index expression",
            Ctx::ClosureBody => "closure body",
        }
    }
}

/// What rustc's `check_unused_delims_expr` is told of a value: where it is, whether a block
/// follows it, where the keyword before it ends and the block after it starts (`left_pos`,
/// `right_pos`), and whether a keyword is before it (`is_kw`).
struct Site {
    ctx: Ctx,
    followed_by_block: bool,
    left: Option<Span>,
    right: Option<Span>,
    is_kw: bool,
}

impl Site {
    fn of(ctx: Ctx) -> Site {
        Site { ctx, followed_by_block: false, left: None, right: None, is_kw: false }
    }
}

/// Where a parenthesized trait object or `impl Trait` is one whose bounds a `+` after it would
/// run into: rustc's `NoBoundsException`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NoBounds {
    /// The type must be parenthesized.
    None,
    /// The type is the last bound: with one bound, its parentheses are not needed.
    OneBound,
}

/// A delimited node's span and, as rustc's `UnusedDelimSuggestion` takes them, the spans from
/// its opening delimiter to what it encloses and from the end of that to its closing one.
struct Delimited {
    value: Span,
    spans: Option<(Span, Span)>,
}

type At = InFile<SyntaxNodePtr>;

struct Check<'c, 'a, 'db, 'w> {
    cx: &'c Cx<'a, 'db>,
    walk: &'c Walk<'w>,
    emitted: Vec<Emitted>,
}

impl Check<'_, '_, '_, '_> {
    /// What rustc's passes check as their walk comes to `node`: `check_expr`, `check_pat`,
    /// `check_ty`, `check_stmt`, `check_param`, `check_arm`, `check_item`, `check_variant` and
    /// `check_generic_arg`.
    fn node(&mut self, file: HirFileId, node: &SyntaxNode) {
        let at = InFile::new(file, SyntaxNodePtr::new(node));
        if let Some(e) = ast::Expr::cast(node.clone()) {
            self.parens_expr(&e, at);
            self.braces_expr(&e, at);
        } else if let Some(pat) = ast::Pat::cast(node.clone()) {
            self.pat(&pat, at);
        } else if let Some(ty) = ast::Type::cast(node.clone()) {
            self.parens_ty(&ty, at);
            self.braces_ty(&ty, at);
        } else if let Some(it) = ast::LetStmt::cast(node.clone()) {
            self.let_stmt(&it, at);
        } else if let Some(it) = ast::StmtList::cast(node.clone()) {
            if let Some(tail) = it.tail_expr() {
                self.statement(tail);
            }
        } else if let Some(it) = ast::ExprStmt::cast(node.clone()) {
            if it.semicolon_token().is_none()
                && let Some(e) = it.expr()
            {
                self.statement(e);
            }
        } else if let Some(it) = ast::MacroStmts::cast(node.clone()) {
            if let Some(tail) = it.expr() {
                self.statement(tail);
            }
        } else if let Some(it) = ast::Param::cast(node.clone()) {
            if let Some(pat) = it.pat() {
                self.parens_pat(&pat, true, false, (false, false), at);
            }
        } else if let Some(it) = ast::MatchArm::cast(node.clone()) {
            if let Some(pat) = it.pat() {
                self.parens_pat(&pat, false, false, (false, false), at);
            }
        } else if let Some(it) = ast::Const::cast(node.clone()) {
            if !in_assoc_list(node) {
                self.item_value(it.body(), at);
            }
        } else if let Some(it) = ast::Static::cast(node.clone()) {
            if !in_assoc_list(node) {
                self.item_value(it.body(), at);
            }
        } else if let Some(it) = ast::Use::cast(node.clone()) {
            if let Some(tree) = it.use_tree() {
                self.use_tree(&tree, &it, at);
            }
        } else if let Some(it) = ast::ConstArg::cast(node.clone()) {
            if node.parent().is_some_and(|it| it.kind() == SyntaxKind::GENERIC_ARG_LIST)
                && let Some(e) = it.expr()
            {
                self.braces_value(&e, &Site::of(Ctx::AnonConst), at);
            }
        } else if let Some(it) = ast::Variant::cast(node.clone())
            && let Some(e) = it.const_arg().and_then(|it| it.expr())
        {
            self.braces_value(&e, &Site::of(Ctx::AnonConst), at);
        }
    }

    /// `UnusedParens::check_expr`: the patterns of `let` and `for`, the scrutinee of an
    /// `if let`, the bodies of match arms, then what both lints check.
    fn parens_expr(&mut self, e: &ast::Expr, at: At) {
        match e {
            ast::Expr::LetExpr(it) => {
                if let Some(pat) = it.pat() {
                    self.parens_pat(&pat, false, false, (true, true), at);
                }
            }
            ast::Expr::ForExpr(it) => {
                if let Some(pat) = it.pat() {
                    self.parens_pat(&pat, false, false, (true, true), at);
                }
            }
            ast::Expr::IfExpr(it) => {
                // `if (let ..)` is an error of lowering; the scrutinee is still linted.
                if let Some(cond) = it.condition().map(peel_parens)
                    && matches!(cond, ast::Expr::LetExpr(_))
                {
                    let site = Site { followed_by_block: true, is_kw: true, ..Site::of(Ctx::LetScrutineeExpr) };
                    self.parens_value(&cond, &site, at);
                    return;
                }
            }
            ast::Expr::MatchExpr(it) => {
                for arm in it.match_arm_list().into_iter().flat_map(|it| it.arms()) {
                    if !self.kept(arm.syntax()) {
                        continue;
                    }
                    if let Some(body) = arm.expr() {
                        let site = Site { is_kw: true, ..Site::of(Ctx::MatchArmExpr) };
                        self.parens_value(&body, &site, at);
                    }
                }
            }
            _ => {}
        }
        self.delim_expr(Lint::Parens, e, at);
    }

    /// `UnusedBraces::check_expr`: what both lints check, and the length of `[x; { 3 }]`.
    fn braces_expr(&mut self, e: &ast::Expr, at: At) {
        self.delim_expr(Lint::Braces, e, at);
        if let ast::Expr::ArrayExpr(it) = e
            && let ast::ArrayExprKind::Repeat { repeat: Some(len), .. } = it.kind()
        {
            self.braces_value(&len, &Site::of(Ctx::AnonConst), at);
        }
    }

    /// `UnusedDelimLint::check_expr`: the value of an expression that takes one.
    fn delim_expr(&mut self, lint: Lint, e: &ast::Expr, at: At) {
        let node = e.syntax();
        let (value, site) = match e {
            ast::Expr::IfExpr(it) => {
                let Some(cond) = it.condition() else { return };
                if matches!(cond, ast::Expr::LetExpr(_)) && !lint.in_pattern_matching() {
                    return;
                }
                let left = it.if_token().map(|it| self.position(node, it.text_range().start() + TextSize::from(2)));
                let right = it.then_branch().map(|it| self.position(node, it.syntax().text_range().start()));
                (cond, Site { ctx: Ctx::IfCond, followed_by_block: true, left, right, is_kw: true })
            }
            ast::Expr::WhileExpr(it) => {
                let Some(cond) = it.condition() else { return };
                if matches!(cond, ast::Expr::LetExpr(_)) && !lint.in_pattern_matching() {
                    return;
                }
                let start = it
                    .label()
                    .map(|it| it.syntax().text_range().start())
                    .or_else(|| it.while_token().map(|it| it.text_range().start()));
                let left = start.map(|it| self.position(node, it + TextSize::from(5)));
                let right = it.loop_body().map(|it| self.position(node, it.syntax().text_range().start()));
                (cond, Site { ctx: Ctx::WhileCond, followed_by_block: true, left, right, is_kw: true })
            }
            ast::Expr::ForExpr(it) => {
                let Some(iter) = it.iterable() else { return };
                let right = it.loop_body().map(|it| self.position(node, it.syntax().text_range().start()));
                (iter, Site { ctx: Ctx::ForIterExpr, followed_by_block: true, left: None, right, is_kw: true })
            }
            ast::Expr::MatchExpr(it) if lint.in_pattern_matching() => {
                let Some(head) = it.expr() else { return };
                let left = it.match_token().map(|it| self.position(node, it.text_range().start() + TextSize::from(5)));
                let site = Site { ctx: Ctx::MatchScrutineeExpr, followed_by_block: true, left, right: None, is_kw: true };
                (head, site)
            }
            ast::Expr::ReturnExpr(it) => {
                let Some(value) = it.expr() else { return };
                let left = it.return_token().map(|it| self.position(node, it.text_range().start() + TextSize::from(3)));
                (value, Site { left, is_kw: true, ..Site::of(Ctx::ReturnValue) })
            }
            ast::Expr::BreakExpr(it) => {
                let Some(value) = it.expr() else { return };
                // `break 'label ({ .. })` needs them: without, the block would be the label's.
                if it.lifetime().is_some()
                    && let ast::Expr::ParenExpr(paren) = expanded(self.cx, value.clone())
                    && paren.expr().is_some_and(|it| precedence::is_block(&expanded(self.cx, it)))
                {
                    return;
                }
                (value, Site { is_kw: true, ..Site::of(Ctx::BreakValue) })
            }
            ast::Expr::IndexExpr(it) => {
                let Some(index) = it.index() else { return };
                (index, Site::of(Ctx::IndexExpr))
            }
            ast::Expr::BinExpr(it) if matches!(it.op_kind(), Some(ast::BinaryOp::Assignment { .. })) => {
                let Some(rhs) = it.rhs() else { return };
                (rhs, Site::of(Ctx::AssignedValue))
            }
            _ => {
                self.args(lint, e, at);
                return;
            }
        };
        self.value(lint, &value, &site, at);
    }

    /// The arguments of a call, a method call or the body of a closure with no return type.
    fn args(&mut self, lint: Lint, e: &ast::Expr, at: At) {
        let (args, ctx, callee_from_expansion): (Vec<ast::Expr>, Ctx, bool) = match e {
            ast::Expr::CallExpr(it) => (
                it.arg_list().into_iter().flat_map(|it| it.args()).collect(),
                Ctx::FunctionArg,
                it.expr().is_some_and(|it| self.from_expansion(it.syntax())),
            ),
            ast::Expr::MethodCallExpr(it) => (
                it.arg_list().into_iter().flat_map(|it| it.args()).collect(),
                Ctx::MethodArg,
                it.name_ref().is_some_and(|it| self.from_expansion(it.syntax())),
            ),
            ast::Expr::ClosureExpr(it) if it.ret_type().is_none() => {
                (it.body().into_iter().collect(), Ctx::ClosureBody, false)
            }
            _ => return,
        };
        // A parenthesized token tree one macro hands another as an expression is not the
        // second macro's author's to care about (rust-lang/rust#47775).
        if self.in_nested_expansion(e.syntax()) {
            return;
        }
        for arg in args {
            if !self.kept(arg.syntax()) {
                continue;
            }
            // Braces can choose which `macro_rules!` arm is taken (rust-lang/rust#158747).
            if callee_from_expansion && self.block_wraps_expanded_expr(&arg) {
                continue;
            }
            self.value(lint, &arg, &Site::of(ctx), at);
        }
    }

    /// `check_stmt` of an expression statement with no `;`: a block's tail, or a block-like
    /// expression before more statements. A statement macro is checked as the statements it
    /// expands to, in its expansion.
    fn statement(&mut self, e: ast::Expr) {
        if matches!(e, ast::Expr::MacroExpr(_)) || !self.kept(e.syntax()) {
            return;
        }
        let at = self.at(e.syntax());
        for lint in [Lint::Parens, Lint::Braces] {
            self.value(lint, &e, &Site::of(Ctx::BlockRetValue), at);
        }
    }

    /// `UnusedParens::check_stmt` of a `let`: its pattern, then its initializer.
    fn let_stmt(&mut self, it: &ast::LetStmt, at: At) {
        if let Some(pat) = it.pat() {
            self.parens_pat(&pat, true, false, (true, false), at);
        }
        let Some(init) = it.initializer() else { return };
        let ctx = match it.let_else() {
            None => Ctx::AssignedValue,
            Some(_) => {
                // `let Some(_) = (x!{}) else { .. }`: what the parentheses hold is a macro's.
                if let ast::Expr::ParenExpr(paren) = expanded(self.cx, init.clone())
                    && let Some(inner) = paren.expr()
                    && self.ctxt(paren.syntax()) != self.ctxt(expanded(self.cx, inner).syntax())
                {
                    return;
                }
                Ctx::AssignedValueLetElse
            }
        };
        self.value(Lint::Parens, &init, &Site::of(ctx), at);
    }

    /// `UnusedDelimLint::check_item`: the value of a `const` or `static` item.
    fn item_value(&mut self, body: Option<ast::Expr>, at: At) {
        let Some(body) = body else { return };
        for lint in [Lint::Parens, Lint::Braces] {
            self.value(lint, &body, &Site::of(Ctx::AssignedValue), at);
        }
    }

    fn value(&mut self, lint: Lint, value: &ast::Expr, site: &Site, at: At) {
        match lint {
            Lint::Parens => self.parens_value(value, site, at),
            Lint::Braces => self.braces_value(value, site, at),
        }
    }

    /// `UnusedParens::check_unused_delims_expr`.
    fn parens_value(&mut self, value: &ast::Expr, site: &Site, at: At) {
        match expanded(self.cx, value.clone()) {
            ast::Expr::ParenExpr(paren) => {
                let Some(inner) = paren.expr() else { return };
                let inner = expanded(self.cx, inner);
                let unused = !precedence::delims_necessary(self.cx, &inner, site.ctx, site.followed_by_block)
                    && !has_outer_attrs(paren.syntax())
                    && !self.from_expansion(paren.syntax())
                    && !(site.ctx == Ctx::LetScrutineeExpr && precedence::is_lazy(&inner))
                    && !(matches!(site.ctx, Ctx::ReturnValue | Ctx::BreakValue) && precedence::is_assign(&inner));
                if !unused {
                    return;
                }
                let Some(delimited) = self.delimited(paren.syntax(), SyntaxKind::L_PAREN, SyntaxKind::R_PAREN)
                else {
                    return;
                };
                self.emit_value(Lint::Parens, delimited, site, at);
            }
            ast::Expr::LetExpr(it) => {
                if let Some(scrutinee) = it.expr() {
                    let site = Site { followed_by_block: site.followed_by_block, ..Site::of(Ctx::LetScrutineeExpr) };
                    self.parens_value(&scrutinee, &site, at);
                }
            }
            _ => {}
        }
    }

    /// `UnusedBraces::check_unused_delims_expr`: a block of one expression, unlabelled and not
    /// `unsafe`, on one line.
    fn braces_value(&mut self, value: &ast::Expr, site: &Site, at: At) {
        match expanded(self.cx, value.clone()) {
            ast::Expr::BlockExpr(block) if precedence::is_plain_block(&block) => {
                let Some(list) = block.stmt_list() else { return };
                let Some(expr) = self.single_expr(&list) else { return };
                if precedence::delims_necessary(self.cx, &expr, site.ctx, site.followed_by_block)
                    || (site.ctx == Ctx::AnonConst
                        && !(matches!(expr, ast::Expr::Literal(_)) && !self.from_expansion(expr.syntax())))
                    || site.ctx == Ctx::ClosureBody
                    || has_attrs(block.syntax())
                    || has_attrs(list.syntax())
                    || self.from_expansion(block.syntax())
                    || self.from_expansion(list.syntax())
                {
                    return;
                }
                let Some(delimited) = self.delimited(list.syntax(), SyntaxKind::L_CURLY, SyntaxKind::R_CURLY)
                else {
                    return;
                };
                if self.cx.snippet(&delimited.value).is_none_or(|it| it.contains('\n')) {
                    return;
                }
                self.emit_value(Lint::Braces, delimited, site, at);
            }
            ast::Expr::LetExpr(it) => {
                if let Some(scrutinee) = it.expr() {
                    let site = Site { followed_by_block: site.followed_by_block, ..Site::of(Ctx::LetScrutineeExpr) };
                    self.braces_value(&scrutinee, &site, at);
                }
            }
            _ => {}
        }
    }

    /// The one statement of a block when it is an expression with no `;` (rustc's
    /// `[StmtKind::Expr(expr)]`), as rustc sees it: a statement macro's expansion in place.
    fn single_expr(&self, list: &ast::StmtList) -> Option<ast::Expr> {
        let empty_stmts = list.syntax().children_with_tokens().filter(|it| it.kind() == SyntaxKind::SEMICOLON);
        if empty_stmts.count() != 0 || list.statements().any(|it| self.kept(it.syntax())) {
            return None;
        }
        let mut e = list.tail_expr().filter(|it| self.kept(it.syntax()))?;
        for _ in 0..MACRO_DEPTH {
            let ast::Expr::MacroExpr(call) = &e else { break };
            let root = call.macro_call().and_then(|it| self.cx.sema.expand_macro_call(&it))?;
            e = match ast::MacroStmts::cast(root.value.clone()) {
                Some(stmts) if stmts.statements().next().is_some() => return None,
                Some(stmts) => stmts.expr()?,
                None => ast::Expr::cast(root.value)?,
            };
        }
        Some(e)
    }

    /// `block_wraps_expanded_expr`: a block written in the source whose one expression is a
    /// macro's.
    fn block_wraps_expanded_expr(&self, value: &ast::Expr) -> bool {
        let ast::Expr::BlockExpr(block) = expanded(self.cx, value.clone()) else { return false };
        precedence::is_plain_block(&block)
            && !self.from_expansion(block.syntax())
            && block
                .stmt_list()
                .and_then(|it| self.single_expr(&it))
                .is_some_and(|it| self.from_expansion(it.syntax()))
    }

    /// `UnusedParens::check_pat`: the parentheses around a pattern's subpatterns.
    fn pat(&mut self, pat: &ast::Pat, at: At) {
        let no_space = (false, false);
        let subpatterns: Vec<ast::Pat> = match pat {
            ast::Pat::TupleStructPat(it) => it.fields().collect(),
            ast::Pat::TuplePat(it) => it.fields().collect(),
            ast::Pat::SlicePat(it) => it.pats().collect(),
            ast::Pat::OrPat(it) => it.pats().collect(),
            ast::Pat::RecordPat(it) => it
                .record_pat_field_list()
                .into_iter()
                .flat_map(|it| it.fields())
                .filter(|it| self.kept(it.syntax()))
                .filter_map(|it| it.pat())
                .collect(),
            // Not `i @ (a | b)` or `box (a | b)` (rust-lang/rust#64106).
            ast::Pat::IdentPat(it) => {
                if let Some(sub) = it.pat() {
                    self.parens_pat(&sub, true, false, no_space, at);
                }
                return;
            }
            ast::Pat::BoxPat(it) => {
                if let Some(sub) = it.pat() {
                    self.parens_pat(&sub, true, false, no_space, at);
                }
                return;
            }
            ast::Pat::DerefPat(it) => {
                if let Some(sub) = it.pat() {
                    self.parens_pat(&sub, true, false, no_space, at);
                }
                return;
            }
            // Not `&(mut x)`: `&mut x` means another thing (rust-lang/rust#55342).
            ast::Pat::RefPat(it) => {
                if let Some(sub) = it.pat() {
                    self.parens_pat(&sub, true, it.mut_token().is_none(), no_space, at);
                }
                return;
            }
            _ => return,
        };
        for sub in subpatterns {
            if self.kept(sub.syntax()) {
                self.parens_pat(&sub, false, false, no_space, at);
            }
        }
    }

    /// `check_unused_parens_pat`.
    fn parens_pat(&mut self, value: &ast::Pat, avoid_or: bool, avoid_mut: bool, keep_space: (bool, bool), at: At) {
        let ast::Pat::ParenPat(paren) = expanded_pat(self.cx, value.clone()) else { return };
        let Some(inner) = paren.pat() else { return };
        match expanded_pat(self.cx, inner) {
            // Kept for readability wherever they are, as in `&(a..=b)`.
            ast::Pat::RangePat(_) => return,
            ast::Pat::OrPat(_) if avoid_or => return,
            ast::Pat::IdentPat(it) if avoid_mut && it.mut_token().is_some() && it.ref_token().is_none() => return,
            _ => {}
        }
        let Some(delimited) = self.delimited(paren.syntax(), SyntaxKind::L_PAREN, SyntaxKind::R_PAREN) else {
            return;
        };
        self.emit(Lint::Parens, delimited, "pattern", keep_space, false, at);
    }

    /// `UnusedParens::check_ty`: a parenthesized type, the length of an array type, and the
    /// parenthesized bounds of a trait object or `impl Trait`.
    fn parens_ty(&mut self, ty: &ast::Type, at: At) {
        match ty {
            ast::Type::ParenType(paren) => {
                if self.cast_followed_by_lt(paren) {
                    return;
                }
                let Some(inner) = paren.ty() else { return };
                let unused = match expanded_ty(self.cx, inner) {
                    ast::Type::ImplTraitType(it) => self.bounds_unused(paren, it.type_bound_list()),
                    ast::Type::DynTraitType(it) => self.bounds_unused(paren, it.type_bound_list()),
                    ast::Type::ForType(it) if matches!(it.ty(), Some(ast::Type::FnPtrType(_))) => {
                        let no_params = it
                            .for_binder()
                            .and_then(|it| it.generic_param_list())
                            .is_none_or(|it| it.generic_params().next().is_none());
                        !self.with_self_ty_parens(paren) || no_params
                    }
                    _ => true,
                };
                if !unused {
                    return;
                }
                let Some(delimited) = self.delimited(paren.syntax(), SyntaxKind::L_PAREN, SyntaxKind::R_PAREN)
                else {
                    return;
                };
                self.emit(Lint::Parens, delimited, "type", (false, false), false, at);
            }
            ast::Type::ArrayType(it) => {
                if let Some(len) = it.const_arg().and_then(|it| it.expr()) {
                    self.parens_value(&len, &Site::of(Ctx::ArrayLenExpr), at);
                }
            }
            ast::Type::DynTraitType(it) => self.paren_bounds(it.type_bound_list(), true, at),
            ast::Type::ImplTraitType(it) => self.paren_bounds(it.type_bound_list(), false, at),
            _ => {}
        }
    }

    /// `UnusedBraces::check_ty`: the length of an array type.
    fn braces_ty(&mut self, ty: &ast::Type, at: At) {
        if let ast::Type::ArrayType(it) = ty
            && let Some(len) = it.const_arg().and_then(|it| it.expr())
        {
            self.braces_value(&len, &Site::of(Ctx::ArrayLenExpr), at);
        }
    }

    /// Whether the parentheses `paren` around a trait object or `impl Trait` of `bounds` are
    /// not needed where it is.
    fn bounds_unused(&self, paren: &ast::ParenType, bounds: Option<ast::TypeBoundList>) -> bool {
        match self.no_bounds(paren.syntax()) {
            Some(NoBounds::None) => false,
            Some(NoBounds::OneBound) => bounds.map_or(0, |it| it.bounds().count()) <= 1,
            None => true,
        }
    }

    /// rustc's `in_no_bounds_pos` of a type: what a `&` or `*const` before it, or being the
    /// return type of a `Fn` bound of a trait object or `impl Trait`, makes of its bounds.
    fn no_bounds(&self, ty: &SyntaxNode) -> Option<NoBounds> {
        let ty = self.stand_in(ty);
        let parent = ty.parent()?;
        let pointee = match ast::RefType::cast(parent.clone()) {
            Some(it) => it.ty(),
            None => ast::PtrType::cast(parent.clone()).and_then(|it| it.ty()),
        };
        if let Some(pointee) = pointee {
            return (*pointee.syntax() == ty).then(|| self.no_bounds(&parent).unwrap_or(NoBounds::OneBound));
        }
        let ret = ast::RetType::cast(parent)?;
        if ret.ty().is_none_or(|it| *it.syntax() != ty) {
            return None;
        }
        let segment = ast::PathSegment::cast(ret.syntax().parent()?)?;
        segment.parenthesized_arg_list()?;
        let path = segment.syntax().parent()?;
        let path_ty = path.parent()?;
        if !ast::Path::can_cast(path.kind()) || !ast::PathType::can_cast(path_ty.kind()) {
            return None;
        }
        let mut bound = path_ty.parent()?;
        if ast::ForType::can_cast(bound.kind()) {
            bound = bound.parent()?;
        }
        let bound = ast::TypeBound::cast(bound)?;
        let list = ast::TypeBoundList::cast(bound.syntax().parent()?)?;
        let owner = list.syntax().parent()?;
        if !ast::DynTraitType::can_cast(owner.kind()) && !ast::ImplTraitType::can_cast(owner.kind()) {
            return None;
        }
        match list.bounds().last() == Some(bound) {
            true => Some(NoBounds::OneBound),
            false => Some(NoBounds::None),
        }
    }

    /// rustc's `with_self_ty_parens`: `paren` is the bounded type of a `where` predicate with
    /// no `for<..>` of its own, where `(for<'a> fn(&'a u8)): Trait` needs them.
    fn with_self_ty_parens(&self, paren: &ast::ParenType) -> bool {
        let node = self.stand_in(paren.syntax());
        node.parent()
            .and_then(ast::WherePred::cast)
            .is_some_and(|pred| pred.for_binder().is_none() && pred.ty().is_some_and(|it| *it.syntax() == node))
    }

    /// rustc's `parens_in_cast_in_lt`: `1 as (i32) < 2` is `1 as i32<2..` without them.
    fn cast_followed_by_lt(&self, paren: &ast::ParenType) -> bool {
        let node = self.stand_in(paren.syntax());
        let Some(cast) = node.parent().and_then(ast::CastExpr::cast) else { return false };
        if cast.ty().is_none_or(|it| *it.syntax() != node) {
            return false;
        }
        let mut current = self.stand_in(cast.syntax());
        loop {
            let Some(bin) = current.parent().and_then(ast::BinExpr::cast) else { return false };
            let Some(op) = bin.op_kind() else { return false };
            if matches!(op, ast::BinaryOp::Assignment { .. }) {
                return false;
            }
            if bin.lhs().is_some_and(|it| *it.syntax() == current) {
                return matches!(
                    op,
                    ast::BinaryOp::CmpOp(ast::CmpOp::Ord { ordering: ast::Ordering::Less, strict: true })
                        | ast::BinaryOp::ArithOp(ast::ArithOp::Shl)
                );
            }
            current = self.stand_in(bin.syntax());
        }
    }

    /// The parenthesized bounds of a trait object or `impl Trait` that need no parentheses:
    /// not a `Fn` bound with a return type before another, nor, in 2015, a first bound of a
    /// path from the root (`dyn (::a::B)`, where `dyn::a::B` is a path).
    fn paren_bounds(&mut self, list: Option<ast::TypeBoundList>, is_dyn: bool, at: At) {
        let Some(list) = list else { return };
        let bounds: Vec<ast::TypeBound> = list.bounds().collect();
        for (i, bound) in bounds.iter().enumerate() {
            let is_last = i + 1 == bounds.len();
            let Some(path) = trait_bound_path(bound) else { continue };
            let fn_with_explicit_ret_ty = path
                .segment()
                .is_some_and(|it| it.parenthesized_arg_list().is_some() && it.ret_type().is_some());
            let dyn2015_exception = is_dyn
                && self.cx.edition == Edition::Edition2015
                && i == 0
                && path.first_segment().is_some_and(|it| it.coloncolon_token().is_some());
            let parenthesized =
                bound.syntax().children_with_tokens().any(|it| it.kind() == SyntaxKind::L_PAREN);
            if !parenthesized || !(is_last || !fn_with_explicit_ret_ty) || dyn2015_exception {
                continue;
            }
            if self.from_expansion(bound.syntax()) {
                continue;
            }
            let span = self.cx.span(InFile::new(self.file(bound.syntax()), bound.syntax().text_range()));
            if !self.cx.snippet(&span).is_some_and(|it| it.starts_with('(') && it.ends_with(')')) {
                continue;
            }
            let spans = Some((span.with_hi(span.lo + 1), span.with_lo(span.hi - 1)));
            self.emit(Lint::Parens, Delimited { value: span, spans }, "type", (false, false), false, at);
        }
    }

    /// `UnusedImportBraces::check_use_tree`: a list of one name or glob, inner lists first.
    fn use_tree(&mut self, tree: &ast::UseTree, item: &ast::Use, at: At) {
        let Some(list) = tree.use_tree_list() else { return };
        let trees: Vec<ast::UseTree> = list.use_trees().collect();
        for it in &trees {
            self.use_tree(it, item, at);
        }
        let [only] = &trees[..] else { return };
        let name = if only.use_tree_list().is_some() {
            return;
        } else if only.star_token().is_some() {
            "*".to_owned()
        } else {
            let Some(name_ref) = only.path().and_then(|it| it.segment()).and_then(|it| it.name_ref()) else {
                return;
            };
            if name_ref.self_token().is_some() {
                return;
            }
            match only.rename() {
                Some(rename) => rename.name().map_or_else(|| "_".to_owned(), |it| self.ident(it.text())),
                None => self.ident(name_ref.text()),
            }
        };
        let file = self.file(item.syntax());
        let span = self.cx.span(InFile::new(file, crate::cx::item_range(item.syntax())));
        let mut diag = Diag::lint("unused_import_braces", format!("braces around {name} is unnecessary"), span);
        super::source_fact(self.cx, &mut diag, at);
        self.emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Early, at));
    }

    /// A name as rustc prints a `Symbol`: `r#` only before a keyword.
    fn ident(&self, text: &str) -> String {
        match text.strip_prefix("r#") {
            Some(name) if !syntax::utils::is_raw_identifier(name, self.cx.edition) => name.to_owned(),
            _ => text.to_owned(),
        }
    }

    /// `emit_unused_delims_expr`: whether to keep a space where the keyword before the value
    /// or the block after it touches it.
    fn emit_value(&mut self, lint: Lint, delimited: Delimited, site: &Site, at: At) {
        let value = &delimited.value;
        let keep_space = (
            site.left.as_ref().is_some_and(|it| it.file == value.file && it.lo >= value.lo),
            site.right.as_ref().is_some_and(|it| it.file == value.file && it.lo <= value.hi),
        );
        self.emit(lint, delimited, site.ctx.describe(), keep_space, site.is_kw, at);
    }

    /// `emit_unused_delims`: the lint on both delimiters, and their removal, with a space where
    /// what was around them would otherwise run together.
    fn emit(&mut self, lint: Lint, delimited: Delimited, item: &str, keep_space: (bool, bool), is_kw: bool, at: At) {
        let message = format!("unnecessary {} around {item}", lint.delims());
        let mut diag = match &delimited.spans {
            Some((lo, hi)) => {
                if hi.is_empty() {
                    return;
                }
                let mut diag = Diag::lint(lint.name(), message, lo.clone());
                diag.primary(hi.clone());
                diag
            }
            None => Diag::lint(lint.name(), message, delimited.value.clone()),
        };
        if let Some((lo, hi)) = delimited.spans {
            let text = self.cx.file_of(&lo).map_or("", |it| self.cx.file_text(it));
            let before = text.get(..lo.lo as usize).unwrap_or("");
            let after = text.get(hi.hi as usize..).unwrap_or("");
            let lo_replace = match ((keep_space.0 || is_kw) && !before.ends_with(' '))
                || before.ends_with(char::is_alphanumeric)
            {
                true => " ",
                false => "",
            };
            let hi_replace = match (keep_space.1 && !after.starts_with(' ')) || after.starts_with(char::is_alphanumeric) {
                true => " ",
                false => "",
            };
            diag.suggestion_with_style(
                vec![(lo, lo_replace.to_owned()), (hi, hi_replace.to_owned())],
                format!("remove these {}", lint.delims()),
                Applicability::MachineApplicable,
                SuggestionStyle::ShowCode,
            );
        }
        super::source_fact(self.cx, &mut diag, at);
        self.emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Early, at));
    }

    /// The span of `node` and, when it is written in the source, those of its delimiters
    /// `open` and `close` with what is between them and what they enclose (rustc's
    /// `span_with_attrs.find_ancestor_inside(value.span)`); `None` when `close` is missing, as
    /// the parser's recovery leaves it.
    fn delimited(&self, node: &SyntaxNode, open: SyntaxKind, close: SyntaxKind) -> Option<Delimited> {
        let elements: Vec<_> = node.children_with_tokens().collect();
        let open_at = elements.iter().position(|it| it.kind() == open)?;
        let close_at = elements.iter().rposition(|it| it.kind() == close).filter(|&it| it > open_at)?;
        let enclosed: Vec<_> = elements[open_at + 1..close_at].iter().filter(|it| !it.kind().is_trivia()).collect();
        let (first, last) = (enclosed.first()?, enclosed.last()?);
        let file = self.file(node);
        let span = |range: TextRange| self.cx.span(InFile::new(file, range));
        let value = span(TextRange::new(elements[open_at].text_range().start(), elements[close_at].text_range().end()));
        let spans = match self.from_expansion(node) {
            true => None,
            false => {
                let inner = span(TextRange::new(first.text_range().start(), last.text_range().end()));
                (inner.file == value.file && value.lo <= inner.lo && inner.hi <= value.hi)
                    .then(|| (value.with_hi(inner.lo), value.with_lo(inner.hi)))
            }
        };
        Some(Delimited { value, spans })
    }

    /// The span rustc gives a position `offset` of the tree `node` is in.
    fn position(&self, node: &SyntaxNode, offset: TextSize) -> Span {
        self.cx.span(InFile::new(self.file(node), TextRange::empty(offset)))
    }

    /// rustc's `e.span.ctxt().outer_expn_data().call_site.from_expansion()`: `node` was made by
    /// a macro whose call was itself made by one.
    fn in_nested_expansion(&self, node: &SyntaxNode) -> bool {
        let file = self.file(node);
        if file.macro_file().is_none() {
            return false;
        }
        let Some(call) = self.ctxt(node).outer_expn(self.cx.db) else { return false };
        let call = hir_expand::MacroCallId::from(call).loc(self.cx.db).to_node(self.cx.db);
        super::from_expansion(self.cx, call.as_ref())
    }

    /// The syntax context of rustc's span of `node`: that of its first token.
    fn ctxt(&self, node: &SyntaxNode) -> SyntaxContext {
        self.file(node).span_map(self.cx.db).span_for_range(node.text_range()).ctx
    }

    /// The node rustc's AST has where `node` is: when `node` is all of what a type, pattern or
    /// expression macro expands to, the place of the call.
    fn stand_in(&self, node: &SyntaxNode) -> SyntaxNode {
        let mut node = node.clone();
        for _ in 0..MACRO_DEPTH {
            if node.parent().is_some() {
                break;
            }
            let Some(call) = self.file(&node).macro_file() else { break };
            let call = call.loc(self.cx.db).to_node(self.cx.db).value;
            match call.parent() {
                Some(wrapper)
                    if matches!(
                        wrapper.kind(),
                        SyntaxKind::MACRO_TYPE | SyntaxKind::MACRO_PAT | SyntaxKind::MACRO_EXPR
                    ) =>
                {
                    node = wrapper;
                }
                _ => break,
            }
        }
        node
    }

    fn file(&self, node: &SyntaxNode) -> HirFileId {
        self.cx.sema.hir_file_for(node)
    }

    fn at(&self, node: &SyntaxNode) -> At {
        InFile::new(self.file(node), SyntaxNodePtr::new(node))
    }

    fn from_expansion(&self, node: &SyntaxNode) -> bool {
        super::from_expansion(self.cx, InFile::new(self.file(node), node))
    }

    /// Whether rustc's walk has `node`: `cfg` keeps it.
    fn kept(&self, node: &SyntaxNode) -> bool {
        self.walk.tree(self.file(node)).is_none_or(|tree| !tree.skipped(node.text_range()))
    }
}

/// What rustc's expanded AST has for `e`: what a macro call expands to, through macro calls
/// that expand to macro calls.
pub(super) fn expanded(cx: &Cx<'_, '_>, e: ast::Expr) -> ast::Expr {
    let mut e = e;
    for _ in 0..MACRO_DEPTH {
        let ast::Expr::MacroExpr(call) = &e else { break };
        let Some(root) = call.macro_call().and_then(|it| cx.sema.expand_macro_call(&it)) else { break };
        let next = match ast::MacroStmts::cast(root.value.clone()) {
            Some(stmts) if stmts.statements().next().is_none() => stmts.expr(),
            Some(_) => None,
            None => ast::Expr::cast(root.value),
        };
        let Some(next) = next else { break };
        e = next;
    }
    e
}

/// [`expanded`] of a pattern.
fn expanded_pat(cx: &Cx<'_, '_>, pat: ast::Pat) -> ast::Pat {
    let mut pat = pat;
    for _ in 0..MACRO_DEPTH {
        let ast::Pat::MacroPat(call) = &pat else { break };
        let Some(next) = call
            .macro_call()
            .and_then(|it| cx.sema.expand_macro_call(&it))
            .and_then(|it| ast::Pat::cast(it.value))
        else {
            break;
        };
        pat = next;
    }
    pat
}

/// [`expanded`] of a type.
pub(super) fn expanded_ty(cx: &Cx<'_, '_>, ty: ast::Type) -> ast::Type {
    let mut ty = ty;
    for _ in 0..MACRO_DEPTH {
        let ast::Type::MacroType(call) = &ty else { break };
        let Some(next) = call
            .macro_call()
            .and_then(|it| cx.sema.expand_macro_call(&it))
            .and_then(|it| ast::Type::cast(it.value))
        else {
            break;
        };
        ty = next;
    }
    ty
}

/// rustc's `peel_parens`.
fn peel_parens(e: ast::Expr) -> ast::Expr {
    let mut e = e;
    while let ast::Expr::ParenExpr(paren) = &e
        && let Some(inner) = paren.expr()
    {
        e = inner;
    }
    e
}

/// The path of a trait bound (rustc's `GenericBound::Trait`), `for<..>` or not.
fn trait_bound_path(bound: &ast::TypeBound) -> Option<ast::Path> {
    match bound.ty()? {
        ast::Type::PathType(it) => it.path(),
        ast::Type::ForType(it) => match it.ty()? {
            ast::Type::PathType(it) => it.path(),
            _ => None,
        },
        _ => None,
    }
}

/// Whether an item is an associated one, which rustc's `check_item` is not called on.
fn in_assoc_list(item: &SyntaxNode) -> bool {
    item.parent().is_some_and(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST)
}

fn is_attr(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT)
}

/// Whether `node` has attributes of its own (rustc's `value.attrs`).
fn has_attrs(node: &SyntaxNode) -> bool {
    node.children().any(|it| is_attr(it.kind()))
}

/// Whether a parenthesized expression has attributes before its `(`: those after it are of
/// what it encloses.
fn has_outer_attrs(paren: &SyntaxNode) -> bool {
    paren
        .children_with_tokens()
        .take_while(|it| it.kind() != SyntaxKind::L_PAREN)
        .any(|it| is_attr(it.kind()))
}
