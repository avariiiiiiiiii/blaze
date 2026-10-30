//! What rustc lints in the crate's bodies (functions, consts, statics, enum discriminants, the
//! closures and inline consts in them): what type checking, match checking, unsafety checking,
//! liveness and the MIR lints find, whatever else failed, and what its late lint passes find,
//! only when nothing failed.
//!
//! - `reachable`: `unreachable_code` (typeck).
//! - `patterns`: what match checking lints (`unreachable_patterns`, `bindings_with_variant_name`,
//!   `irrefutable_let_patterns`).
//! - `recursion`: `unconditional_recursion` (the MIR lints of calls, and of drops after drop
//!   elaboration).
//! - `unsafety`: `unused_unsafe` (`check_unsafety`).
//! - `liveness`: `unused_variables`, `unused_assignments` (`check_liveness`).
//! - `panics`: `arithmetic_overflow`, `unconditional_panic` (the known-panics lint).
//! - `types`, `calls`: rustc's late lint passes of expressions, statements and patterns.
//!
//! Follows rustc's `compiler/rustc_interface/src/passes.rs` (`run_required_analyses`: each body
//! owner's `check_unsafety`, which builds its MIR and so checks its matches first, then
//! `mir_borrowck`, which first runs the MIR lints of `mir_promoted`, `check_liveness` and, when
//! rustc makes code, `mir_drops_elaborated_and_const_checked`, in the order of
//! `hir_crate_items(()).body_owners`), `compiler/rustc_lint/src/late.rs` (`LateContextAndPass`:
//! each node's `check_*` before its children's, every pass at each node) and
//! `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedLateLintModPass`: the order of the passes)
//! (MIT or Apache-2.0).

mod calls;
mod liveness;
mod panics;
mod patterns;
mod reachable;
mod recursion;
mod types;
mod unsafety;

use blaze_items::{Fact, ItemId};
use hir::{InFile, db::HirDatabase};
use hir_expand::MacroCallKind;
use hir_def::{
    AdtId, DefWithBodyId, Lookup, ModuleDefId,
    expr_store::{Body, BodySourceMap, ExpressionStore},
    hir::{Array, BindingId, Expr, ExprId, Pat, PatId, Statement},
    nameres::demand::every_body,
    signatures::FunctionSignature,
    src::HasSource as _,
};
use hir_ty::{
    InferBodyId, InferenceDiagnostic, InferenceResult, PathLoweringDiagnostic, TyLoweringDiagnostic,
    mir::{MirBody, MirSpan},
};
use rustc_hash::FxHashMap;
use span::FileId;
use syntax::{SyntaxNodePtr, TextRange, TextSize};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// What type checking, match checking, unsafety checking, liveness and the MIR lints lint in
/// the crate's bodies, given whatever else failed: each body in rustc's order, its typeck's
/// lints at [`Phase::Bodies`], the rest at [`Phase::Mir`]. `codegen` is whether rustc makes
/// code of the crate (`--emit` asks for more than metadata: `cargo build`, not `cargo check`),
/// which runs the lints after drop elaboration on every body, not only on those of consts.
pub(crate) fn before_abort(cx: &Cx<'_, '_>, codegen: bool) -> Vec<Found> {
    let mut found = Vec::new();
    for owner in body_owners(cx) {
        let Some(mut b) = BodyCx::new(cx, owner) else { continue };
        b.elaborated = codegen || b.is_const_context();
        found.extend(reachable::lints(&b));
        found.extend(patterns::lints(&b));
        found.extend(unsafety::lints(&b));
        found.extend(reachable::after_calls(&b));
        found.extend(recursion::calls(&b));
        found.extend(liveness::lints(&b));
        found.extend(recursion::drops(&b));
        found.extend(panics::lints(&b));
    }
    found
}

/// What rustc's late lint passes lint in bodies (`unused_comparisons`, `path_statements`,
/// `overflowing_literals`, ..), run only when nothing failed: each pass at each node of each
/// body, in rustc's order.
pub(crate) fn after_abort(cx: &Cx<'_, '_>) -> Vec<Found> {
    let mut passes = late_passes();
    let mut found = Vec::new();
    for owner in body_owners(cx) {
        let Some(b) = BodyCx::new(cx, owner) else { continue };
        Late { b: &b, passes: &mut passes, out: &mut found }.body();
    }
    found
}

/// One body being linted, with what it was lowered to and inferred as.
pub(crate) struct BodyCx<'a, 'db> {
    pub(crate) cx: &'a Cx<'a, 'db>,
    pub(crate) owner: DefWithBodyId,
    pub(crate) body: &'db Body,
    pub(crate) source_map: &'db BodySourceMap,
    pub(crate) infer: &'db InferenceResult<'db>,
    /// rustc's `tainted_by_errors`: type checking the body gave an error, so rustc builds no
    /// THIR of it and runs none of its MIR lints (match checking, unsafety, liveness, known
    /// panics).
    pub(crate) tainted: bool,
    /// rustc runs `mir_drops_elaborated_and_const_checked` on the body before it aborts, so
    /// its lints (the known-panics lint, a `Drop` impl's `unconditional_recursion`): it makes
    /// code, or the body is in a const context. Set by [`before_abort`].
    pub(crate) elaborated: bool,
    /// The expressions of the body in the order rustc visits them, each before those in it;
    /// those of an anonymous const in it (an array length) are the const's.
    exprs: Vec<ExprId>,
}

impl<'a, 'db> BodyCx<'a, 'db> {
    /// The body of `owner`, if it has one written (a variant has one only when its
    /// discriminant is).
    pub(crate) fn new(cx: &'a Cx<'a, 'db>, owner: DefWithBodyId) -> Option<BodyCx<'a, 'db>> {
        let db = cx.db;
        let (body, source_map) = Body::with_source_map(db, owner);
        let body: &'db Body = body;
        if matches!(body.expr(body.root_expr()), Expr::Missing) && body.params.is_empty() {
            return None;
        }
        let infer = InferenceResult::of(db, owner);
        let tainted = infer.diagnostics().iter().any(taints);
        let mut b = BodyCx { cx, owner, body, source_map, infer, tainted, elaborated: false, exprs: Vec::new() };
        let mut exprs = Vec::new();
        b.preorder(body.root_expr(), &mut exprs);
        b.exprs = exprs;
        Some(b)
    }

    pub(crate) fn db(&self) -> &'db dyn HirDatabase {
        self.cx.db
    }

    pub(crate) fn store(&self) -> &'db ExpressionStore {
        &self.body.store
    }

    /// The expressions of the body, each before those in it, in the order rustc visits them.
    pub(crate) fn exprs(&self) -> &[ExprId] {
        &self.exprs
    }

    /// The node `expr` is written as, if it is written (not made by lowering).
    pub(crate) fn expr_node(&self, expr: ExprId) -> Option<InFile<SyntaxNodePtr>> {
        let source = self.source_map.expr_syntax(expr).ok()?;
        Some(source.map(|it| it.syntax_node_ptr()))
    }

    pub(crate) fn pat_node(&self, pat: PatId) -> Option<InFile<SyntaxNodePtr>> {
        let source = self.source_map.pat_syntax(pat).ok()?;
        Some(source.map(|it| it.syntax_node_ptr()))
    }

    /// The pattern a binding is first bound by, or `self`.
    pub(crate) fn binding_node(&self, binding: BindingId) -> Option<InFile<SyntaxNodePtr>> {
        let is_self = self.body.self_param.is_some_and(|it| it.formal == binding || it.user_written == binding);
        if is_self {
            return self.source_map.self_param_syntax().map(|it| it.map(|it| it.syntax_node_ptr()));
        }
        let pat = *self.source_map.patterns_for_binding(binding).first()?;
        self.pat_node(pat)
    }

    pub(crate) fn expr_span(&self, expr: ExprId) -> Option<Span> {
        Some(self.cx.span_of(self.expr_node(expr)?))
    }

    pub(crate) fn pat_span(&self, pat: PatId) -> Option<Span> {
        Some(self.cx.span_of(self.pat_node(pat)?))
    }

    /// The item the body is of (the function, const, static or variant): where rustc emits a
    /// lint of the body as a whole.
    pub(crate) fn owner_node(&self) -> InFile<SyntaxNodePtr> {
        owner_node(self.db(), self.owner)
    }

    /// The MIR the body is built to, before rustc's runtime passes, if it can be built.
    pub(crate) fn mir(&self) -> Option<&'db MirBody<'db>> {
        self.db().mir_body(InferBodyId::DefWithBodyId(self.owner)).ok()
    }

    /// The MIR of the body after drop elaboration (`MirPhase::Runtime`), if it can be built.
    pub(crate) fn runtime_mir(&self) -> Option<&'db MirBody<'db>> {
        self.db().runtime_mir_body(InferBodyId::DefWithBodyId(self.owner)).ok()
    }

    /// rustc's `hir_body_const_context` is some: a const, a static, a discriminant or a
    /// `const fn`.
    pub(crate) fn is_const_context(&self) -> bool {
        match self.owner {
            DefWithBodyId::FunctionId(it) => FunctionSignature::of(self.db(), it).is_const(),
            DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => true,
        }
    }

    /// The span rustc's MIR has for what `span` is of: an expression, the end of one (the last
    /// character, rustc's `end_point` of a scope), a pattern, a binding or `self`.
    pub(crate) fn mir_span(&self, span: MirSpan) -> Option<Span> {
        let node = self.mir_node(span)?;
        let MirSpan::ExprEnd(_) = span else { return Some(self.cx.span_of(node)) };
        let last = self.cx.syntax(node).value.last_token()?;
        let end = last.text_range().end();
        let len = TextSize::of(last.text().chars().next_back()?);
        Some(self.cx.span(InFile::new(node.file_id, TextRange::new(end - len, end))))
    }

    /// The node what `span` is of is written as (of [`MirSpan::ExprEnd`], the whole expression).
    pub(crate) fn mir_node(&self, span: MirSpan) -> Option<InFile<SyntaxNodePtr>> {
        match span {
            MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => self.expr_node(expr),
            MirSpan::PatId(pat) => self.pat_node(pat),
            MirSpan::BindingId(binding) => self.binding_node(binding),
            MirSpan::SelfParam => {
                let param = self.source_map.self_param_syntax()?;
                Some(param.map(|it| it.syntax_node_ptr()))
            }
            MirSpan::Unknown => None,
        }
    }

    /// A type of the body as rustc prints it, through the type of its owner.
    pub(crate) fn ty(&self, ty: hir_ty::next_solver::Ty<'db>) -> String {
        let db = self.db();
        let owner = match self.owner {
            DefWithBodyId::FunctionId(it) => hir::Function::from(it).ret_type(db),
            DefWithBodyId::ConstId(it) => hir::Const::from(it).ty(db),
            DefWithBodyId::StaticId(it) => hir::Static::from(it).ty(db),
            DefWithBodyId::VariantId(it) => hir::EnumVariant::from(it).parent_enum(db).ty(db),
        };
        self.cx.ty(&owner.derived_from_next_solver(ty))
    }

    /// rustc's `Span::in_external_macro` of `node`: written by a macro of another crate, or
    /// by an attribute or derive macro, rather than handed to it.
    pub(crate) fn in_external_macro(&self, node: InFile<SyntaxNodePtr>) -> bool {
        let db = self.db();
        let Some(call) = node.file_id.macro_file() else { return false };
        if let Some((_, ctxt)) = node.map(|it| it.text_range()).original_node_file_range_opt(db)
            && ctxt.is_root()
        {
            return false;
        }
        let loc = call.loc(db);
        match loc.kind {
            MacroCallKind::FnLike { .. } => hir::Crate::from(loc.def.krate) != self.cx.krate,
            _ => true,
        }
    }

    /// The item whose code the body is, with what to call it in a fact.
    fn item(&self) -> (ItemId, hir::ModuleDef) {
        match self.owner {
            DefWithBodyId::FunctionId(it) => (ItemId::Function(it), ModuleDefId::FunctionId(it).into()),
            DefWithBodyId::ConstId(it) => (ItemId::Const(it), ModuleDefId::ConstId(it).into()),
            DefWithBodyId::StaticId(it) => (ItemId::Static(it), ModuleDefId::StaticId(it).into()),
            DefWithBodyId::VariantId(it) => {
                let parent = it.lookup(self.db()).parent;
                (ItemId::Adt(AdtId::EnumId(parent)), ModuleDefId::EnumVariantId(it).into())
            }
        }
    }

    /// Records on `diag` that it read the code of the body.
    pub(crate) fn code_fact(&self, diag: &mut Diag) {
        let (item, def) = self.item();
        let what = format!("the code of `{}`", self.cx.def_path(def));
        diag.fact(Fact::Code(self.cx.key(item)), what);
    }

    /// A lint of the body found, its level decided at `at`, with the fact that it read the
    /// body's code.
    pub(crate) fn lint(&self, mut diag: Diag, phase: Phase, at: InFile<SyntaxNodePtr>) -> Found {
        self.code_fact(&mut diag);
        Found::lint(diag, phase, at)
    }

    /// The expressions from `expr` down, in rustc's order ([`Late`]'s).
    fn preorder(&self, expr: ExprId, out: &mut Vec<ExprId>) {
        out.push(expr);
        let mut children = Vec::new();
        child_exprs(self.store(), expr, &mut children);
        for child in children {
            stdx::ensure_sufficient_stack(|| self.preorder(child, out));
        }
    }
}

/// Whether rustc's type checking of a body taints it for an inference diagnostic: every one
/// is an error but an elided lifetime in a path, which is the `elided_lifetimes_in_paths` lint
/// where it is not a hard error.
fn taints(diagnostic: &InferenceDiagnostic) -> bool {
    let path = match diagnostic {
        InferenceDiagnostic::PathDiagnostic { diag, .. } => diag,
        InferenceDiagnostic::TyDiagnostic { diag: TyLoweringDiagnostic::PathDiagnostic { diag, .. } } => diag,
        _ => return true,
    };
    !matches!(path, PathLoweringDiagnostic::ElidedLifetimesInPath { hard_error: false, .. })
}

/// The expressions directly in `expr` that are of its body, in the order rustc's HIR visitor
/// walks them (`rustc_hir::intravisit::walk_expr`), those in patterns included: a `let`'s
/// initializer before its pattern, a `match` arm's pattern before its guard and value. An array
/// length is an anonymous const of its own, and not of the body.
fn child_exprs(store: &ExpressionStore, expr: ExprId, out: &mut Vec<ExprId>) {
    let pat = |pat: PatId, out: &mut Vec<ExprId>| store.walk_exprs_in_pat(pat, |it| out.push(it));
    match store.expr(expr) {
        Expr::Let { pat: p, expr } => {
            out.push(expr);
            pat(p, out);
        }
        Expr::Block { statements, tail, .. } => {
            for statement in statements {
                match statement {
                    Statement::Let { pat: p, initializer, else_branch, .. } => {
                        out.extend(*initializer);
                        pat(*p, out);
                        out.extend(*else_branch);
                    }
                    Statement::Expr { expr, .. } => out.push(*expr),
                    Statement::Item(_) => {}
                }
            }
            out.extend(tail);
        }
        Expr::Match { expr, arms } => {
            out.push(expr);
            for arm in arms.iter() {
                pat(arm.pat, out);
                out.extend(arm.guard);
                out.push(arm.expr);
            }
        }
        Expr::Closure { args, body, .. } => {
            for arg in args.iter() {
                pat(*arg, out);
            }
            out.push(body);
        }
        Expr::Assignment { target, value } => {
            pat(target, out);
            out.push(value);
        }
        Expr::Array(Array::Repeat { initializer, .. }) => out.push(initializer),
        _ => store.walk_child_exprs_without_pats(expr, |it| out.push(it)),
    }
}

/// The bodies of the crate in the order rustc gives their lints (`body_owners`, each before
/// the items in it): by where they are written, the files in the order of the modules they
/// hold.
fn body_owners(cx: &Cx<'_, '_>) -> Vec<DefWithBodyId> {
    let db = cx.db;
    let mut rank: FxHashMap<FileId, usize> = FxHashMap::default();
    for module in crate::checks::modules(cx) {
        let file = module.definition_source_file_id(db).original_file(db).file_id(db);
        let next = rank.len();
        rank.entry(file).or_insert(next);
    }
    let mut owners: Vec<(usize, TextSize, usize, DefWithBodyId)> = every_body(db, cx.krate.base())
        .into_iter()
        .enumerate()
        .map(|(index, owner)| {
            let at = owner_node(db, owner).map(|it| it.text_range());
            let at = at.original_node_file_range_rooted(db);
            let file = at.file_id.file_id(db);
            (rank.get(&file).copied().unwrap_or(usize::MAX), at.range.start(), index, owner)
        })
        .collect();
    owners.sort_by_key(|&(file, at, index, _)| (file, at, index));
    owners.into_iter().map(|(.., owner)| owner).collect()
}

fn owner_node(db: &dyn HirDatabase, owner: DefWithBodyId) -> InFile<SyntaxNodePtr> {
    match owner {
        DefWithBodyId::FunctionId(it) => it.lookup(db).ast_ptr(db).map(|it| it.syntax_node_ptr()),
        DefWithBodyId::ConstId(it) => it.lookup(db).ast_ptr(db).map(|it| it.syntax_node_ptr()),
        DefWithBodyId::StaticId(it) => it.lookup(db).ast_ptr(db).map(|it| it.syntax_node_ptr()),
        DefWithBodyId::VariantId(it) => it.lookup(db).ast_ptr(db).map(|it| it.syntax_node_ptr()),
    }
}

/// A late lint pass of rustc's, of what it checks in bodies (`rustc_lint::LateLintPass`): each
/// method is called at each node of each body, before the methods at the nodes in it
/// (`check_expr_post` after), as `LateContextAndPass` calls them.
pub(crate) trait LatePass {
    /// rustc's name of the pass (`TypeLimits`), which places it in [`PASS_ORDER`].
    fn name(&self) -> &'static str;
    /// The body, before anything in it.
    fn check_body(&mut self, _b: &BodyCx<'_, '_>, _out: &mut Vec<Found>) {}
    fn check_expr(&mut self, _b: &BodyCx<'_, '_>, _expr: ExprId, _out: &mut Vec<Found>) {}
    fn check_expr_post(&mut self, _b: &BodyCx<'_, '_>, _expr: ExprId, _out: &mut Vec<Found>) {}
    /// Statement `index` of the block `block`, before what is in it.
    fn check_stmt(&mut self, _b: &BodyCx<'_, '_>, _block: ExprId, _index: usize, _out: &mut Vec<Found>) {}
    /// The `let` statement `index` of the block `block`, after [`LatePass::check_stmt`] of it.
    fn check_local(&mut self, _b: &BodyCx<'_, '_>, _block: ExprId, _index: usize, _out: &mut Vec<Found>) {}
    /// Arm `index` of the `match` `expr`, before its pattern.
    fn check_arm(&mut self, _b: &BodyCx<'_, '_>, _expr: ExprId, _index: usize, _out: &mut Vec<Found>) {}
    fn check_pat(&mut self, _b: &BodyCx<'_, '_>, _pat: PatId, _out: &mut Vec<Found>) {}
}

/// The late passes of rustc's that look at bodies, in the order `BuiltinCombinedLateLintModPass`
/// lists them, which is the order rustc gives their lints at one node.
const PASS_ORDER: &[&str] = &[
    "ForLoopsOverFallibles",
    "DropForgetUseless",
    "InvalidFromUtf8",
    "PathStatements",
    "LetUnderscore",
    "InvalidReferenceCasting",
    "ImplicitAutorefs",
    "UnusedResults",
    "UnitBindings",
    "NonShorthandFieldPatterns",
    "UnusedAllocation",
    "PtrNullChecks",
    "MutableTransmutes",
    "TypeLimits",
    "InvalidValue",
    "DerefNullPtr",
    "ShadowedIntoIter",
    "DanglingPointers",
    "NonPanicFmt",
    "NoopMethodCall",
    "EnumIntrinsicsNonEnums",
    "InvalidAtomicOrdering",
    "AsmLabels",
    "MapUnitFn",
    "InteriorMutableConsts",
    "IfLetRescope",
    "StaticMutRefs",
    "FunctionCastsAsInteger",
    "CheckTransmutes",
    "ImplicitProvenanceCasts",
    "RawBorrowsViaReferences",
];

/// Every late pass of the children, in rustc's order.
fn late_passes() -> Vec<Box<dyn LatePass>> {
    let mut passes: Vec<Box<dyn LatePass>> = Vec::new();
    passes.extend(types::passes());
    passes.extend(calls::passes());
    let place = |pass: &Box<dyn LatePass>| {
        let name = pass.name();
        PASS_ORDER
            .iter()
            .position(|it| *it == name)
            .unwrap_or_else(|| panic!("the late pass `{name}` is not in `PASS_ORDER`"))
    };
    passes.sort_by_key(|it| place(it));
    passes
}

/// rustc's `LateContextAndPass` over one body: every pass at each node, in its visitor's order.
struct Late<'s, 'a, 'db> {
    b: &'s BodyCx<'a, 'db>,
    passes: &'s mut [Box<dyn LatePass>],
    out: &'s mut Vec<Found>,
}

impl Late<'_, '_, '_> {
    fn body(&mut self) {
        let b = self.b;
        for pass in self.passes.iter_mut() {
            pass.check_body(b, self.out);
        }
        for param in b.body.params.iter() {
            self.pat(param.user_written);
        }
        self.expr(b.body.root_expr());
    }

    fn expr(&mut self, expr: ExprId) {
        let b = self.b;
        for pass in self.passes.iter_mut() {
            pass.check_expr(b, expr, self.out);
        }
        stdx::ensure_sufficient_stack(|| self.expr_children(expr));
        for pass in self.passes.iter_mut() {
            pass.check_expr_post(b, expr, self.out);
        }
    }

    fn expr_children(&mut self, expr: ExprId) {
        let b = self.b;
        let store = b.store();
        match store.expr(expr) {
            Expr::Let { pat, expr } => {
                self.expr(expr);
                self.pat(pat);
            }
            Expr::Block { statements, tail, .. } => {
                for (index, statement) in statements.iter().enumerate() {
                    if let Statement::Item(_) = statement {
                        continue;
                    }
                    for pass in self.passes.iter_mut() {
                        pass.check_stmt(b, expr, index, self.out);
                    }
                    match statement {
                        Statement::Let { pat, initializer, else_branch, .. } => {
                            for pass in self.passes.iter_mut() {
                                pass.check_local(b, expr, index, self.out);
                            }
                            if let Some(it) = initializer {
                                self.expr(*it);
                            }
                            self.pat(*pat);
                            if let Some(it) = else_branch {
                                self.expr(*it);
                            }
                        }
                        Statement::Expr { expr, .. } => self.expr(*expr),
                        Statement::Item(_) => {}
                    }
                }
                if let Some(tail) = tail {
                    self.expr(tail);
                }
            }
            Expr::Match { expr: scrutinee, arms } => {
                self.expr(scrutinee);
                for (index, arm) in arms.iter().enumerate() {
                    for pass in self.passes.iter_mut() {
                        pass.check_arm(b, expr, index, self.out);
                    }
                    self.pat(arm.pat);
                    if let Some(guard) = arm.guard {
                        self.expr(guard);
                    }
                    self.expr(arm.expr);
                }
            }
            Expr::Closure { args, body, .. } => {
                for arg in args.iter() {
                    self.pat(*arg);
                }
                self.expr(body);
            }
            Expr::Assignment { target, value } => {
                self.pat(target);
                self.expr(value);
            }
            Expr::Array(Array::Repeat { initializer, .. }) => self.expr(initializer),
            _ => {
                let mut children = Vec::new();
                store.walk_child_exprs_without_pats(expr, |it| children.push(it));
                for child in children {
                    self.expr(child);
                }
            }
        }
    }

    fn pat(&mut self, pat: PatId) {
        let b = self.b;
        for pass in self.passes.iter_mut() {
            pass.check_pat(b, pat, self.out);
        }
        let store = b.store();
        match store.pat(pat) {
            Pat::Lit(expr) | Pat::Expr(expr) => self.expr(expr),
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
                store.walk_pats_shallow(pat, |it| children.push(it));
                for child in children {
                    self.pat(child);
                }
            }
        }
    }
}