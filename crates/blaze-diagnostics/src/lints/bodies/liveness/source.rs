//! What rustc's liveness lints take from a body's source rather than its MIR: where each
//! binding is introduced (`BindingForm::Var`'s `introductions` and `opt_match_place`), the
//! span rustc's MIR building gives an assignment, the names and spans of captures, the string
//! literals of a body, the source uses of a binding, and the suggestions written from the
//! source (`maybe_suggest_unit_pattern_typo`, `annotate_mut_binding_to_immutable_binding`).
//!
//! Follows rustc's `compiler/rustc_mir_transform/src/liveness.rs`,
//! `compiler/rustc_mir_build/src/builder/matches/mod.rs` (`declare_bindings`),
//! `compiler/rustc_mir_build/src/builder/block.rs` (the `opt_match_place` of a `let`),
//! `compiler/rustc_mir_build/src/builder/expr/stmt.rs` (the span of an assignment),
//! `compiler/rustc_ast_lowering/src/expr.rs` (`destructure_assign`) and
//! `compiler/rustc_middle/src/ty/closure.rs` (`CapturedPlace::to_string`, `get_path_span`)
//! (MIT or Apache-2.0).

use hir::{HasVisibility as _, InFile, ItemInNs};
use hir_def::{
    AdtId, HasModule as _, ItemContainerId, Lookup as _,
    hir::{BindingAnnotation, BindingId, Expr, ExprId, ExprOrPatId, Literal, Pat, PatId, Statement},
    resolver::ValueNs,
    src::HasSource as _,
};
use hir_ty::{
    ValueTyDefId,
    closure_analysis::{CapturedPlace, ProjectionKind},
    mir::MirSpan,
    next_solver::{Ty, TyKind},
};
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::inherent::IntoKind as _;
use syntax::{
    AstNode as _, SyntaxNodePtr, TextRange,
    ast::{self, HasAttrs as _},
};

use crate::{
    lints::bodies::{BodyCx, types::util},
    model::Span,
};

/// rustc's `opt_match_place` of a binding: what its pattern is matched against.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Decl {
    /// `let x = init;`, which builds `init` into the binding (`(None, initializer)`).
    Simple(ExprId),
    /// A pattern matched against what `MirSpan` is of: a `let` with a pattern or an `else`,
    /// a match arm, a `let` expression or a parameter (`(Some(place), span)`).
    Matched(MirSpan),
}

/// What the source of a body tells about its bindings and assignments, for the body and the
/// closures in it.
pub(super) struct Source {
    decls: FxHashMap<BindingId, Decl>,
    /// The place an assignment `place = value` assigns, as the span its MIR assigns it at,
    /// with the assignment: the span rustc gives the assignment is the assignment's.
    assigns: FxHashMap<MirSpan, ExprId>,
    /// The span of what an assignment assigns, by its value, for those whose value is an
    /// operator rustc builds into the place (`x = x + 1`).
    assigned_by_value: FxHashMap<ExprId, MirSpan>,
}

impl Source {
    pub(super) fn new(b: &BodyCx<'_, '_>) -> Source {
        let store = b.store();
        let mut source =
            Source { decls: FxHashMap::default(), assigns: FxHashMap::default(), assigned_by_value: FxHashMap::default() };
        for &param in b.body.params.iter() {
            source.matched(b, param, MirSpan::PatId(param));
        }
        for (id, expr) in store.exprs() {
            match expr {
                Expr::Block { statements, .. } => {
                    for statement in statements {
                        let Statement::Let { pat, initializer: Some(init), else_branch, .. } = *statement else {
                            continue;
                        };
                        match store.pat(pat) {
                            Pat::Bind { id: binding, subpat: None }
                                if else_branch.is_none()
                                    && matches!(
                                        store[binding].mode,
                                        BindingAnnotation::Unannotated | BindingAnnotation::Mutable
                                    ) =>
                            {
                                source.decls.insert(binding, Decl::Simple(init));
                            }
                            _ => source.matched(b, pat, MirSpan::ExprId(init)),
                        }
                    }
                }
                Expr::Match { expr: scrutinee, arms } => {
                    for arm in arms {
                        source.matched(b, arm.pat, MirSpan::ExprId(scrutinee));
                    }
                }
                Expr::Let { pat, expr: scrutinee } => source.matched(b, pat, MirSpan::ExprId(scrutinee)),
                Expr::Closure { args, .. } => {
                    for &arg in args {
                        source.matched(b, arg, MirSpan::PatId(arg));
                    }
                }
                Expr::Assignment { target, value } => {
                    let place = match store.pat(target) {
                        Pat::Path(_) => MirSpan::PatId(target),
                        Pat::Expr(place) => MirSpan::ExprId(place),
                        _ => continue,
                    };
                    source.assigns.insert(place, id);
                    if let Expr::BinaryOp { op: Some(_), .. } = store.expr(value) {
                        source.assigned_by_value.insert(value, place);
                    }
                }
                _ => {}
            }
        }
        source
    }

    fn matched(&mut self, b: &BodyCx<'_, '_>, pat: PatId, span: MirSpan) {
        b.store().walk_bindings_in_pat(pat, |binding| {
            self.decls.entry(binding).or_insert(Decl::Matched(span));
        });
    }

    pub(super) fn decl(&self, binding: BindingId) -> Option<Decl> {
        self.decls.get(&binding).copied()
    }

    /// The assignment whose place is assigned at `span`, if it assigns all of its target.
    pub(super) fn assignment(&self, span: MirSpan) -> Option<ExprId> {
        self.assigns.get(&span).copied()
    }

    /// The span the place an assignment of `value` assigns is assigned at.
    pub(super) fn assigned_by(&self, value: ExprId) -> Option<MirSpan> {
        self.assigned_by_value.get(&value).copied()
    }
}

/// Where a binding is introduced: rustc's `BindingForm::Var` of its local.
pub(super) struct Var {
    pub(super) name: String,
    /// rustc's `def_span`: where the first pattern binding it is.
    pub(super) def: Span,
    pub(super) node: InFile<SyntaxNodePtr>,
    /// Each pattern binding it, and whether it is a field's shorthand (`S { x }`).
    pub(super) introductions: Vec<(Span, bool)>,
}

/// The binding as rustc's MIR building introduces it, unless it was made by lowering
/// (`from_compiler_desugaring`) or is not named.
pub(super) fn var(b: &BodyCx<'_, '_>, binding: BindingId) -> Option<Var> {
    let data = &b.store()[binding];
    if data.name.is_missing() || data.name.is_generated() {
        return None;
    }
    let mut introductions = Vec::new();
    let mut first = None;
    for &pat in b.source_map.patterns_for_binding(binding) {
        let Some(node) = b.pat_node(pat) else { continue };
        let (span, shorthand) = introduction(b, node);
        first.get_or_insert(node);
        introductions.push((span, shorthand));
    }
    let node = first?;
    Some(Var {
        name: data.name.display(b.db(), b.cx.edition).to_string(),
        def: introductions[0].0.clone(),
        node,
        introductions,
    })
}

/// The span of a binding's pattern as rustc's THIR has it: from its start to the end of its
/// name (`ref mut x` of `ref mut x @ Some(_)`), and whether it is a field's shorthand.
fn introduction(b: &BodyCx<'_, '_>, node: InFile<SyntaxNodePtr>) -> (Span, bool) {
    let syntax = b.cx.syntax(node);
    let Some(pat) = ast::IdentPat::cast(syntax.value.clone()) else { return (b.cx.span_of(node), false) };
    let range = syntax.value.text_range();
    let end = pat.name().map_or(range.end(), |it| it.syntax().text_range().end());
    let shorthand = pat
        .syntax()
        .parent()
        .and_then(ast::RecordPatField::cast)
        .is_some_and(|field| field.colon_token().is_none());
    (b.cx.span(InFile::new(node.file_id, TextRange::new(range.start(), end))), shorthand)
}

/// The span rustc's MIR has for `span` and the node its lint level is decided at: of a
/// binding's pattern, where its name ends ([`introduction`]); of an expression, with the
/// parentheses around it.
pub(super) fn mir_span(b: &BodyCx<'_, '_>, span: MirSpan) -> Option<(Span, InFile<SyntaxNodePtr>)> {
    match span {
        MirSpan::ExprId(expr) => Some((util::span(b, expr)?, b.expr_node(expr)?)),
        MirSpan::PatId(pat) if matches!(b.store().pat(pat), Pat::Bind { .. }) => {
            let node = b.pat_node(pat)?;
            Some((introduction(b, node).0, node))
        }
        _ => Some((b.mir_span(span)?, b.mir_node(span)?)),
    }
}

/// The span rustc gives the assignment `assign`, `place = value`: the whole assignment, or
/// only its place when what the place holds is dropped first (`build_drop_and_replace`).
pub(super) fn assignment_span(
    b: &BodyCx<'_, '_>,
    assign: ExprId,
    place: MirSpan,
    drops: bool,
) -> Option<(Span, InFile<SyntaxNodePtr>)> {
    let node = b.expr_node(assign)?;
    let span = match drops {
        true => mir_span(b, place)?.0,
        false => util::span(b, assign)?,
    };
    Some((span, node))
}

/// rustc's `CapturedPlace::to_string`: the captured variable with what the capture
/// projects out of it (`*x`, `x.field`, `x.0`).
pub(super) fn capture_name(b: &BodyCx<'_, '_>, capture: &CapturedPlace) -> String {
    let db = b.db();
    let mut name = b.store()[capture.captured_local()].name.display(db, b.cx.edition).to_string();
    for (i, projection) in capture.place.projections.iter().enumerate() {
        match projection.kind {
            ProjectionKind::Deref => name = format!("*{name}"),
            ProjectionKind::Field { field_idx, variant_idx } => {
                match capture.place.ty_before_projection(i).kind() {
                    TyKind::Adt(def, _) => {
                        let fields = match def.def_id() {
                            AdtId::StructId(it) => hir::Struct::from(it).fields(db),
                            AdtId::UnionId(it) => hir::Union::from(it).fields(db),
                            AdtId::EnumId(it) => hir::Enum::from(it)
                                .variants(db)
                                .get(variant_idx as usize)
                                .map(|it| it.fields(db))
                                .unwrap_or_default(),
                        };
                        if let Some(field) = fields.get(field_idx as usize) {
                            name = format!("{name}.{}", field.name(db).display(db, b.cx.edition));
                        }
                    }
                    _ => name = format!("{name}.{field_idx}"),
                }
            }
            ProjectionKind::Index | ProjectionKind::Subslice | ProjectionKind::UnwrapUnsafeBinder => {}
        }
    }
    name
}

/// rustc's `CapturedPlace::get_path_span`: the expression the captured path is written in.
pub(super) fn capture_span(b: &BodyCx<'_, '_>, capture: &CapturedPlace) -> Option<Span> {
    match capture.info.sources.first()?.final_source().unpack() {
        ExprOrPatId::ExprId(expr) => util::span(b, expr),
        ExprOrPatId::PatId(pat) => b.pat_span(pat),
    }
}

/// The expressions of the body whose MIR is built from `root`, with neither the closures
/// nor the inline consts in it, which have MIR of their own.
fn own_exprs(b: &BodyCx<'_, '_>, root: ExprId, mut f: impl FnMut(ExprId, Expr<'_>)) {
    let store = b.store();
    let mut stack = vec![root];
    while let Some(expr) = stack.pop() {
        let it = store.expr(expr);
        f(expr, it);
        if expr != root && matches!(it, Expr::Closure { .. } | Expr::Const(_)) {
            continue;
        }
        store.walk_child_exprs(expr, |child| stack.push(child));
    }
}

/// The string literals of the body built from `root`, in source order, with their values:
/// rustc's string constants of the MIR (`LiteralFinder`).
pub(super) fn string_literals(b: &BodyCx<'_, '_>, root: ExprId) -> Vec<(Span, String)> {
    let mut found = Vec::new();
    own_exprs(b, root, |expr, it| {
        if let Expr::Literal(Literal::String(value)) = it
            && let Some(span) = util::span(b, expr)
        {
            found.push((span, value.as_str().to_owned()));
        }
    });
    found.sort_by_key(|(span, _)| (span.file.clone(), span.lo));
    found
}

/// The bindings a path of the body built from `root` names: rustc's `is_local_used_in_source`.
pub(super) fn used_in_source(b: &BodyCx<'_, '_>, root: ExprId) -> FxHashSet<BindingId> {
    let mut used = FxHashSet::default();
    own_exprs(b, root, |expr, it| {
        if let Expr::Path(_) = it
            && let Some(ValueNs::LocalBinding(binding)) = util::path_value(b, expr)
        {
            used.insert(binding);
        }
    });
    used
}

/// The unit variant or constant a binding named `name` of type `ty` may have been meant to
/// be: rustc's `maybe_suggest_unit_pattern_typo`, as the kind, the name and the path to write.
pub(super) fn pattern_typo<'db>(
    b: &BodyCx<'_, 'db>,
    consts: &[(String, Ty<'db>, String)],
    name: &str,
    ty: Ty<'db>,
) -> Option<(&'static str, String, String)> {
    let db = b.db();
    let mut peeled = ty;
    while let TyKind::Ref(_, inner, _) = peeled.kind() {
        peeled = inner;
    }
    if let TyKind::Adt(def, _) = peeled.kind() {
        let units: Vec<(String, &'static str, hir::ModuleDef)> = match def.def_id() {
            AdtId::EnumId(it) => hir::Enum::from(it)
                .variants(db)
                .into_iter()
                .filter(|it| it.kind(db) == hir::StructKind::Unit)
                .map(|it| (it.name(db).as_str().to_owned(), "variant", hir::ModuleDef::EnumVariant(it)))
                .collect(),
            AdtId::StructId(it) => {
                let it = hir::Struct::from(it);
                match it.kind(db) == hir::StructKind::Unit {
                    true => vec![(it.name(db).as_str().to_owned(), "struct", hir::ModuleDef::Adt(hir::Adt::Struct(it)))],
                    false => Vec::new(),
                }
            }
            AdtId::UnionId(_) => Vec::new(),
        };
        let names: Vec<&str> = units.iter().map(|(name, ..)| name.as_str()).collect();
        if let Some(found) = crate::lints::edit_distance::find_best_match_for_name(&names, name, None)
            && let Some((item, kind, def)) = units.iter().find(|(it, ..)| it == found)
            && let Some(path) = untrimmed_path(b, *def)
        {
            return Some((kind, item.clone(), path));
        }
    }
    let same: Vec<&(String, Ty<'db>, String)> = consts.iter().filter(|(_, it, _)| *it == ty).collect();
    let names: Vec<&str> = same.iter().map(|(name, ..)| name.as_str()).collect();
    let found = crate::lints::edit_distance::find_best_match_for_name(&names, name, None)?;
    let (item, _, path) = same.iter().find(|(it, ..)| it == found)?;
    Some(("constant", item.clone(), path.clone()))
}

/// The free constants of the crate the body can name, with their types and paths: what
/// rustc's `maybe_suggest_unit_pattern_typo` looks through.
pub(super) fn constants<'db>(b: &BodyCx<'_, 'db>) -> Vec<(String, Ty<'db>, String)> {
    let db = b.db();
    let here = hir::Module::from(b.owner.module(db));
    let mut found = Vec::new();
    for module in b.cx.krate.modules(db) {
        for def in module.declarations(db) {
            let hir::ModuleDef::Const(it) = def else { continue };
            let Some(name) = it.name(db) else { continue };
            if !it.is_visible_from(db, here) {
                continue;
            }
            let Some(ty) = db.value_ty(ValueTyDefId::ConstId(it.into())) else { continue };
            let Some(path) = untrimmed_path(b, def) else { continue };
            found.push((name.as_str().to_owned(), ty.skip_binder(), path));
        }
    }
    found
}

/// rustc's `with_no_trimmed_paths!(def_path_str(def))` of a variant, struct or constant: its
/// path from the crate's root.
fn untrimmed_path(b: &BodyCx<'_, '_>, def: hir::ModuleDef) -> Option<String> {
    let db = b.db();
    let config = hir::FindPathConfig {
        prefer_no_std: false,
        prefer_prelude: false,
        prefer_absolute: false,
        allow_unstable: true,
    };
    let path = b.cx.krate.root_module(db).find_path(db, ItemInNs::Values(def), config)?;
    Some(path.display(db, b.cx.edition).to_string())
}

/// rustc's `annotate_mut_binding_to_immutable_binding`: the parts that make `mut a: &T`, whose
/// dead assignment `a = &b` is `assign`, into `a: &mut T` and `*a = b`.
pub(super) fn mut_reference_param(
    b: &BodyCx<'_, '_>,
    param: PatId,
    assign: ExprId,
) -> Option<Vec<(Span, String)>> {
    let store = b.store();
    let Pat::Bind { id, subpat: None } = store.pat(param) else { return None };
    if store[id].mode != BindingAnnotation::Mutable {
        return None;
    }
    let pat = ast::IdentPat::cast(b.cx.syntax(b.pat_node(param)?).value)?;
    let ast::Type::RefType(ty) = ast::Param::cast(pat.syntax().parent()?)?.ty()? else { return None };
    let Expr::Assignment { target, value } = store.expr(assign) else { return None };
    let Expr::Ref { expr: inner, rawness: hir_def::hir::Rawness::Ref, .. } = store.expr(value) else { return None };
    let node = b.pat_node(param)?;
    let at = |range: TextRange| b.cx.span(InFile::new(node.file_id, range));
    let mut parts = Vec::new();
    if ty.mut_token().is_none() {
        let pre = if ty.lifetime().is_some() { " " } else { "" };
        let inner_ty = ty.ty()?.syntax().text_range();
        parts.push((at(TextRange::empty(inner_ty.start())), format!("{pre}mut ")));
    }
    let name = pat.name()?.syntax().text_range();
    parts.push((at(TextRange::new(pat.syntax().text_range().start(), name.start())), String::new()));
    let lhs = b.pat_span(target)?;
    parts.push((lhs.shrink_to_lo(), "*".to_owned()));
    let rhs = util::span(b, value)?;
    let inner = util::span(b, inner)?;
    parts.push((rhs.with_hi(inner.lo), String::new()));
    Some(parts)
}

/// rustc's checks that keep `check_liveness` from running on a body: an intrinsic, and a
/// function of an `#[automatically_derived]` trait impl.
pub(super) fn skipped(b: &BodyCx<'_, '_>) -> bool {
    let db = b.db();
    let container = match b.owner {
        hir_def::DefWithBodyId::FunctionId(it) => {
            if hir_def::signatures::FunctionSignature::is_intrinsic(db, it) {
                return true;
            }
            it.lookup(db).container
        }
        hir_def::DefWithBodyId::ConstId(it) => it.lookup(db).container,
        hir_def::DefWithBodyId::StaticId(_) | hir_def::DefWithBodyId::VariantId(_) => return false,
    };
    let ItemContainerId::ImplId(impl_) = container else { return false };
    let impl_ = impl_.lookup(db).source(db).value;
    impl_.trait_().is_some()
        && impl_.attrs().any(|it| it.as_simple_atom().is_some_and(|it| it == "automatically_derived"))
}
