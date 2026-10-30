//! E0277: a trait bound that does not hold, in rustc's words: the message, label and notes of
//! the trait's `#[rustc_on_unimplemented]` or `#[diagnostic::on_unimplemented]` (read from the
//! trait's attributes, `{Self}`, `{This}` and the trait's generic names filled in), else "the
//! trait bound `T: Trait` is not satisfied" and "the trait `Trait` is not implemented for `T`";
//! "consider annotating `S` with `#[derive(Debug)]`" for a local type and a derivable trait;
//! "this trait has no implementations"; the "required for `X` to implement `Y`" chain of the
//! impls that needed the bound; and "required by a bound in `f`" on the callee's bound, the
//! error pointed at the one argument the bound is of, as rustc's typeck adjusts it.
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/traits/
//! fulfillment_errors.rs` (`report_selection_error`, `get_standard_error_message`,
//! `try_to_add_help_message`), `suggestions.rs` (`note_obligation_cause_code`,
//! `get_explanation_based_on_obligation`, `can_suggest_derive`, `suggest_derive`),
//! `on_unimplemented.rs` (`OnUnimplementedDirective::evaluate`, the format string's
//! arguments), and `compiler/rustc_hir_typeck/src/fn_ctxt/adjust_fulfillment_errors.rs`
//! (`blame_specific_arg_if_possible`) (MIT or Apache-2.0).
//!
//! What the front end does not give yet (requested in `v2/requests/diagnostics.md`): the impl
//! each "required for" step went through (rustc points at a local impl's header and the bound
//! that failed), the obligation's cause (rustc's `ObligationCauseCode`, here found again from
//! the syntax the bound was registered at), whether a span is of a format string's
//! desugaring, and the ambiguous goals rustc reports as E0283.

use hir::{HasSource as _, InFile, LangItem, PathResolution};
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, TextRange,
    ast::{self, HasAttrs as _, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use crate::{
    cx::{Cx, def_range},
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
    print,
};

/// rustc's `diagnostic_width`, past which an explanation is a help and not a label.
const DIAGNOSTIC_WIDTH: usize = 140;

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnimplementedTrait<'_>) -> Vec<Found> {
    let leaf = &d.trait_predicate;
    let root = d.parent_trait_predicates.last().unwrap_or(leaf);
    let leaf_ref = leaf.trait_ref();
    let leaf_trait = leaf_ref.trait_();
    let leaf_self = leaf_ref.get_type_argument(0);
    let root_ref = root.trait_ref();
    let root_self = root_ref.get_type_argument(0);
    let main = match (&leaf_self, &root_self) {
        (Some(leaf_self), Some(root_self))
            if root_ref.trait_() != leaf_trait
                && !is_lang(cx, root_ref.trait_(), LangItem::Unsize)
                && (cx.ty(leaf_self) == cx.ty(&root_self.strip_references())
                    || cx.ty(leaf_self) == cx.ty(root_self)) =>
        {
            root
        }
        _ => leaf,
    };
    let main_is_leaf = std::ptr::eq(main, leaf);
    let main_ref = main.trait_ref();
    let direct = d.parent_trait_predicates.is_empty() || !main_is_leaf;

    let at = cx.syntax(d.span.map(|it| it.syntax_node_ptr()));
    let cause = Cause::of(cx, &at, root_self.as_ref());
    let custom = on_unimplemented(cx, main, direct);
    let message = custom.message.clone().unwrap_or_else(|| {
        format!("the trait bound `{}` is not satisfied", print::predicate(cx, main))
    });
    let mut diag = Diag::error(Some("E0277"), message, cause.span.clone());

    let explanation = explanation(cx, leaf);
    let derivable = can_suggest_derive(cx, leaf);
    let ty_span = match leaf_self.as_ref().and_then(|it| it.as_adt()) {
        Some(adt) if is_local_adt(cx, adt) && !derivable => adt_def_span(cx, adt),
        _ => None,
    };
    match &custom.label {
        Some(label) => {
            diag.span_label(cause.span.clone(), label.clone());
            let is_param = leaf_self.as_ref().is_some_and(|it| it.as_type_param(cx.db).is_some());
            if !is_param {
                match ty_span.clone() {
                    Some(at) => diag.span_help(at, explanation),
                    None => diag.help(explanation),
                };
            }
        }
        None if explanation.len() > DIAGNOSTIC_WIDTH || ty_span.is_some() => {
            diag.span_label(cause.span.clone(), "unsatisfied trait bound");
            match ty_span.clone() {
                Some(at) => diag.span_help(at, explanation),
                None => diag.help(explanation),
            };
        }
        None => {
            diag.span_label(cause.span.clone(), explanation);
        }
    }
    if !(main_is_leaf && derivable) {
        for note in &custom.notes {
            diag.note(note.clone());
        }
    }
    cause.label_call(&mut diag);
    if derivable {
        suggest_derive(cx, leaf, &mut diag);
    }
    if is_local_trait(cx, leaf_trait)
        && hir::Impl::all_for_trait(cx.db, leaf_trait).is_empty()
        && !leaf_trait.is_auto(cx.db)
        && leaf.polarity() == hir::PredicatePolarity::Positive
        && let Some(src) = cx.sema.source(leaf_trait)
    {
        let range = src.as_ref().map(|it| def_range(it.syntax()));
        diag.span_help(cx.span(range), "this trait has no implementations, consider adding one");
    }
    note_derived(cx, d, &mut diag);
    if let Some(callee) = cause.callee {
        where_clause_note(cx, &mut diag, callee, root_ref.trait_(), root_self.as_ref());
    }

    for pred in std::iter::once(leaf).chain(&d.parent_trait_predicates) {
        let tr = pred.trait_ref();
        if let Some(self_ty) = tr.get_type_argument(0) {
            let what = format!(
                "whether `{}` implements `{}`",
                cx.ty(&self_ty),
                print::trait_path(cx, &tr)
            );
            diag.fact_opt(cx.implements_fact(tr.trait_(), &self_ty), what);
        }
    }
    code_fact(cx, &mut diag, d.span.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's `get_explanation_based_on_obligation`: "the trait `Trait` is not implemented for
/// `T`".
fn explanation(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>) -> String {
    let tr = pred.trait_ref();
    if pred.polarity() == hir::PredicatePolarity::Negative {
        return format!("the trait bound `{}` is not satisfied", print::predicate(cx, pred));
    }
    let self_ty = tr.get_type_argument(0);
    let desc = match &self_ty {
        Some(ty) if ty.as_callable(cx.db).is_some_and(|it| matches!(it.kind(), hir::CallableKind::Function(_))) && !ty.is_fn_ptr_like() => " fn item",
        Some(ty) if ty.is_closure() => " closure",
        _ => "",
    };
    let unstable = if is_unstable(cx, tr.trait_()) { "nightly-only, unstable " } else { "" };
    let self_text = self_ty.as_ref().map(|it| cx.ty(it)).unwrap_or_default();
    format!(
        "the {unstable}trait `{}` is not implemented for{desc} `{self_text}`",
        print::trait_path(cx, &tr)
    )
}

/// Where rustc points a bound of a call that does not hold: at the one argument whose type
/// the bound is of, the callee then labelled "required by a bound introduced by this call";
/// else where the bound was registered.
pub(super) struct Cause {
    pub(super) span: Span,
    /// The expression the span is of.
    pub(super) node: InFile<SyntaxNode>,
    /// The span of the callee, when the error was moved to an argument.
    pub(super) callee_span: Option<Span>,
    /// The function whose bound it is, when the bound was registered at a call of it.
    pub(super) callee: Option<hir::Function>,
}

impl Cause {
    pub(super) fn of<'db>(
        cx: &Cx<'_, 'db>,
        at: &InFile<SyntaxNode>,
        self_ty: Option<&hir::Type<'db>>,
    ) -> Cause {
        let plain = || Cause {
            span: cx.span(at.as_ref().map(|it| it.text_range())),
            node: at.clone(),
            callee_span: None,
            callee: None,
        };
        let (callee_range, callee, args): (TextRange, Option<hir::Function>, Vec<ast::Expr>) =
            if let Some(path) = ast::PathExpr::cast(at.value.clone())
                && let Some(call) = path.syntax().parent().and_then(ast::CallExpr::cast)
                && call.expr().is_some_and(|it| it.syntax() == path.syntax())
            {
                let callee = path.path().and_then(|it| cx.sema.resolve_path(&it)).and_then(|it| {
                    match it {
                        PathResolution::Def(hir::ModuleDef::Function(f)) => Some(f),
                        _ => None,
                    }
                });
                let args = call.arg_list().map(|it| it.args().collect()).unwrap_or_default();
                (path.syntax().text_range(), callee, args)
            } else if let Some(call) = ast::MethodCallExpr::cast(at.value.clone()) {
                let Some(name) = call.name_ref() else { return plain() };
                let args = call.arg_list().map(|it| it.args().collect()).unwrap_or_default();
                (name.syntax().text_range(), cx.sema.resolve_method_call(&call), args)
            } else {
                return plain();
            };
        let callee_span = cx.span(at.with_value(callee_range));
        let blamed: Vec<&ast::Expr> = match self_ty {
            Some(self_ty) => {
                let want = cx.ty(self_ty);
                args.iter()
                    .filter(|arg| {
                        cx.sema.type_of_expr(arg).is_some_and(|it| {
                            let ty = it.original;
                            cx.ty(&ty) == want || cx.ty(&ty.strip_references()) == want
                        })
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        match blamed.as_slice() {
            [arg] => Cause {
                span: cx.span(at.with_value(arg.syntax().text_range())),
                node: at.with_value(arg.syntax().clone()),
                callee_span: Some(callee_span),
                callee,
            },
            _ => Cause { span: callee_span, node: at.clone(), callee_span: None, callee },
        }
    }

    /// rustc's "required by a bound introduced by this call" on the callee, when the error is
    /// on one of its arguments.
    pub(super) fn label_call(&self, diag: &mut Diag) {
        if let Some(callee) = &self.callee_span
            && callee != &self.span
        {
            diag.span_label(callee.clone(), "required by a bound introduced by this call");
        }
    }
}

/// rustc's `ObligationCauseCode::ImplDerived` notes: for each impl that needed the bound, from
/// the innermost out, "required for `X` to implement `Y`", the steps of a trait already named
/// counted as "N redundant requirements hidden".
fn note_derived(cx: &Cx<'_, '_>, d: &hir::UnimplementedTrait<'_>, diag: &mut Diag) {
    let parents = &d.parent_trait_predicates;
    let required = |pred: &hir::TraitPredicate<'_>| {
        let tr = pred.trait_ref();
        let self_ty = tr.get_type_argument(0).map(|it| cx.ty(&it)).unwrap_or_default();
        let modifier = match pred.polarity() {
            hir::PredicatePolarity::Positive => "",
            hir::PredicatePolarity::Negative => "!",
        };
        format!("required for `{self_ty}` to implement `{modifier}{}`", print::trait_path(cx, &tr))
    };
    let mut seen: Vec<hir::Trait> = vec![d.trait_predicate.trait_ref().trait_()];
    let mut i = 0;
    while i < parents.len() {
        let trait_ = parents[i].trait_ref().trait_();
        diag.note(required(&parents[i]));
        if !seen.contains(&trait_) {
            seen.push(trait_);
        }
        let mut j = i + 1;
        while j < parents.len() && seen.contains(&parents[j].trait_ref().trait_()) {
            j += 1;
        }
        let count = j - i - 1;
        if count > 0 {
            let s = if count == 1 { "" } else { "s" };
            diag.note(format!("{count} redundant requirement{s} hidden"));
            diag.note(required(&parents[j - 1]));
        }
        if let Some(next) = parents.get(j) {
            seen.push(next.trait_ref().trait_());
        }
        i = j;
    }
}

/// rustc's note on the bound of `callee` that did not hold: "required by a bound in `f`", on
/// the bound written in `f`'s generics or where clause; "required by an implicit `Sized`
/// bound in `f`" on the type parameter when it is `Sized`'s and `f` writes no such bound.
/// When `f` writes several bounds of the trait and the front end does not say which one it
/// was, it is not named.
pub(super) fn where_clause_note<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    callee: hir::Function,
    trait_: hir::Trait,
    self_ty: Option<&hir::Type<'db>>,
) {
    let Some(src) = cx.sema.source(callee) else { return };
    let name = print::def_path(cx, hir::ModuleDef::Function(callee));
    let item = &src.value;
    let bounds: Vec<ast::TypeBound> = bounds_of(item)
        .into_iter()
        .filter(|bound| bound_trait(cx, bound) == Some(trait_))
        .collect();
    let ident = item.name().map(|it| cx.span(src.with_value(it.syntax().text_range())));
    let (at, descr, label) = match bounds.as_slice() {
        [bound] => (
            cx.span(src.with_value(bound.syntax().text_range())),
            format!("required by a bound in `{name}`"),
            format!("required by this bound in `{name}`"),
        ),
        [] if is_lang(cx, trait_, LangItem::Sized) => {
            let params: Vec<ast::TypeParam> = item
                .generic_param_list()
                .map(|it| it.type_or_const_params().filter_map(|p| match p {
                    ast::TypeOrConstParam::Type(it) if !is_maybe_sized(&it) => Some(it),
                    _ => None,
                }).collect())
                .unwrap_or_default();
            let param = match (params.as_slice(), self_ty.and_then(|it| it.as_type_param(cx.db))) {
                (_, Some(param)) => params
                    .iter()
                    .find(|it| it.name().is_some_and(|n| n.text() == param.name(cx.db).as_str())),
                ([only], None) => Some(only),
                _ => None,
            };
            let Some(param) = param else { return };
            let range = param.name().map_or(param.syntax().text_range(), |it| it.syntax().text_range());
            (
                cx.span(src.with_value(range)),
                format!("required by an implicit `Sized` bound in `{name}`"),
                format!("required by the implicit `Sized` requirement on this type parameter in `{name}`"),
            )
        }
        _ => return,
    };
    let mut span = crate::model::MultiSpan::of(at.clone());
    if let Some(ident) = ident
        && !overlaps(&ident, &at)
        && !same_line(cx, &ident, &at)
    {
        span.labels.push((ident, "required by a bound in this function".to_owned()));
    }
    span.labels.push((at, label));
    diag.span_note(span, descr);
    diag.fact_opt(
        Some(blaze_items::Fact::Signature(cx.key(blaze_items::ItemId::Function(
            match hir::FunctionId::try_from(callee) {
                Ok(id) => id,
                Err(()) => return,
            },
        )))),
        format!("the signature of `{name}`"),
    );
}

/// The bounds written on a function's type parameters and in its where clause, in order.
fn bounds_of(item: &ast::Fn) -> Vec<ast::TypeBound> {
    let mut out = Vec::new();
    if let Some(params) = item.generic_param_list() {
        for param in params.type_or_const_params() {
            if let ast::TypeOrConstParam::Type(param) = param
                && let Some(list) = param.type_bound_list()
            {
                out.extend(list.bounds());
            }
        }
    }
    if let Some(clause) = item.where_clause() {
        for pred in clause.predicates() {
            if let Some(list) = pred.type_bound_list() {
                out.extend(list.bounds());
            }
        }
    }
    out
}

/// The trait a bound names.
fn bound_trait(cx: &Cx<'_, '_>, bound: &ast::TypeBound) -> Option<hir::Trait> {
    let ast::Type::PathType(ty) = bound.ty()? else { return None };
    match cx.sema.resolve_path(&ty.path()?)? {
        PathResolution::Def(hir::ModuleDef::Trait(it)) => Some(it),
        _ => None,
    }
}

/// Whether a type parameter is written `?Sized`.
fn is_maybe_sized(param: &ast::TypeParam) -> bool {
    param.type_bound_list().is_some_and(|list| {
        list.bounds().any(|it| it.question_mark_token().is_some())
    })
}

fn overlaps(a: &Span, b: &Span) -> bool {
    a.file == b.file && a.lo < b.hi && b.lo < a.hi
}

/// Whether the end of `a` is on the line `b` starts on (rustc compares `ident.span.hi()` with
/// `span.lo()`).
fn same_line(cx: &Cx<'_, '_>, a: &Span, b: &Span) -> bool {
    if a.file != b.file {
        return false;
    }
    let Some(file) = cx.file_of(a) else { return true };
    let text = cx.file_text(file);
    let (lo, hi) = (a.hi.min(b.lo) as usize, a.hi.max(b.lo) as usize);
    text.get(lo..hi).is_none_or(|between| !between.contains('\n'))
}

/// What the trait's `#[rustc_on_unimplemented]` or `#[diagnostic::on_unimplemented]` says of
/// the predicate: rustc's `OnUnimplementedNote`.
#[derive(Default)]
pub(super) struct Custom {
    pub(super) message: Option<String>,
    pub(super) label: Option<String>,
    pub(super) notes: Vec<String>,
}

/// A part of the attribute: `word`, `name = "value"` or `name(..)`.
#[derive(Debug, Clone, PartialEq)]
enum Meta {
    Word(String),
    Value(String, String),
    List(String, Vec<Meta>),
}

/// rustc's `OnUnimplementedDirective::evaluate` of the attributes of the trait of `pred`.
fn on_unimplemented(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>, direct: bool) -> Custom {
    let tr = pred.trait_ref();
    let trait_ = tr.trait_();
    let Some(src) = cx.sema.source(trait_) else { return Custom::default() };
    let args = Args::of(cx, pred);
    let mut custom = Custom::default();
    for attr in src.value.attrs() {
        let Some(ast::Meta::TokenTreeMeta(meta)) = attr.meta() else { continue };
        let Some(path) = meta.path() else { continue };
        let path = path.syntax().text().to_string();
        let is_rustc = path == "rustc_on_unimplemented";
        if !is_rustc && path != "diagnostic::on_unimplemented" {
            continue;
        }
        let Some(tt) = meta.token_tree() else { continue };
        let metas = parse_metas(tt.syntax());
        // Of the directive and its `on(..)`s, each holding one, later ones override: rustc
        // walks them from the last `on` back to the directive, so the first `on` that holds
        // wins and the directive's own fields are the default.
        let mut commands: Vec<(Option<&Meta>, Vec<&Meta>)> = vec![(None, Vec::new())];
        for meta in &metas {
            match meta {
                Meta::List(name, inner) if name == "on" && is_rustc => {
                    let (cond, fields) = inner.split_first().map_or((None, Vec::new()), |(c, f)| {
                        (Some(c), f.iter().collect())
                    });
                    commands.push((cond, fields));
                }
                _ => commands[0].1.push(meta),
            }
        }
        let mut found = Custom::default();
        let mut notes = Vec::new();
        for (cond, fields) in commands.iter().rev() {
            if let Some(cond) = cond
                && !args.holds(cond, direct)
            {
                continue;
            }
            let mut command_notes = Vec::new();
            for field in fields {
                let Meta::Value(name, value) = field else { continue };
                let text = args.format(value);
                match name.as_str() {
                    "message" => found.message = Some(text),
                    "label" => found.label = Some(text),
                    "note" => command_notes.push(text),
                    _ => {}
                }
            }
            notes.extend(command_notes);
        }
        // rustc collects the commands' notes from the directive to the last `on`, each
        // command's in its order.
        let mut ordered = Vec::new();
        for (cond, fields) in &commands {
            if let Some(cond) = cond
                && !args.holds(cond, direct)
            {
                continue;
            }
            for field in fields {
                if let Meta::Value(name, value) = field
                    && name == "note"
                {
                    ordered.push(args.format(value));
                }
            }
        }
        debug_assert_eq!(ordered.len(), notes.len());
        custom.message = custom.message.or(found.message);
        custom.label = custom.label.or(found.label);
        custom.notes.extend(ordered);
    }
    custom
}

/// The parts of an attribute's token tree, `(a, b = "c", d(e))`.
fn parse_metas(tt: &SyntaxNode) -> Vec<Meta> {
    let mut out = Vec::new();
    let mut name: Option<String> = None;
    let mut eq = false;
    let mut items = tt.children_with_tokens().peekable();
    // The opening and closing delimiters.
    items.next();
    for item in items {
        match item {
            NodeOrToken::Node(node) if node.kind() == SyntaxKind::TOKEN_TREE => {
                if let Some(name) = name.take() {
                    out.push(Meta::List(name, parse_metas(&node)));
                }
                eq = false;
            }
            NodeOrToken::Node(_) => {}
            NodeOrToken::Token(token) => match token.kind() {
                kind if kind.is_trivia() => {}
                SyntaxKind::COMMA | SyntaxKind::R_PAREN | SyntaxKind::R_BRACK | SyntaxKind::R_CURLY => {
                    if let Some(name) = name.take() {
                        out.push(Meta::Word(name));
                    }
                    eq = false;
                }
                SyntaxKind::EQ => eq = true,
                SyntaxKind::STRING => {
                    let value = ast::String::cast(token.clone())
                        .and_then(|it| it.value().ok().map(|it| it.into_owned()))
                        .unwrap_or_default();
                    if let (Some(n), true) = (name.take(), eq) {
                        out.push(Meta::Value(n, value));
                    }
                    eq = false;
                }
                _ => {
                    let text = token.text().to_owned();
                    name = Some(match name.take() {
                        Some(prefix) => prefix + &text,
                        None => text,
                    });
                }
            },
        }
    }
    if let Some(name) = name {
        out.push(Meta::Word(name));
    }
    out
}

/// What the format strings and conditions of `on_unimplemented` are of.
struct Args {
    /// The self type as diagnostics print it, for `{Self}`.
    self_ty: String,
    /// What `Self = ".."` is compared with: the type with its whole path, its type's generic
    /// form, and its kind (`{union}`).
    self_matches: Vec<String>,
    /// The trait's name, for `{This}`.
    this: String,
    trait_path: String,
    /// The trait's generic parameters and what they are, for `{T}` and `T = ".."`.
    generics: Vec<(String, String)>,
    /// Whether the self type is an ADT of the crate being checked (`crate_local`).
    crate_local: bool,
}

impl Args {
    fn of(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>) -> Args {
        let tr = pred.trait_ref();
        let trait_ = tr.trait_();
        let self_ty = tr.get_type_argument(0);
        let mut self_matches = Vec::new();
        let mut crate_local = false;
        if let Some(ty) = &self_ty {
            self_matches.push(cx.ty(ty));
            if let Some(adt) = ty.as_adt() {
                crate_local = is_local_adt(cx, adt);
                if let Some(path) = full_path(cx, adt) {
                    let args: Vec<String> = ty.type_arguments().map(|it| cx.ty(&it)).collect();
                    let params: Vec<String> = hir::GenericDef::Adt(adt)
                        .type_or_const_params(cx.db)
                        .into_iter()
                        .map(|it| it.name(cx.db).as_str().to_owned())
                        .collect();
                    if args.is_empty() {
                        self_matches.push(path.clone());
                    } else {
                        self_matches.push(format!("{path}<{}>", args.join(", ")));
                        self_matches.push(format!("{path}<{}>", params.join(", ")));
                    }
                }
                self_matches.push(
                    match adt {
                        hir::Adt::Struct(_) => "{struct}",
                        hir::Adt::Enum(_) => "{enum}",
                        hir::Adt::Union(_) => "{union}",
                    }
                    .to_owned(),
                );
            } else if ty.is_int_or_uint() {
                self_matches.push("{integral}".to_owned());
            } else if ty.is_float() {
                self_matches.push("{float}".to_owned());
            }
        }
        let mut generics = Vec::new();
        let lifetimes = hir::GenericDef::Trait(trait_).lifetime_params(cx.db).len();
        for (i, param) in hir::GenericDef::Trait(trait_).type_or_const_params(cx.db).into_iter().enumerate() {
            let name = param.name(cx.db).as_str().to_owned();
            if name == "Self" {
                continue;
            }
            // The arguments are the self type, then the trait's own, lifetimes last.
            if lifetimes == 0
                && let Some(arg) = tr.get_type_argument(i)
            {
                generics.push((name, cx.ty(&arg)));
            }
        }
        Args {
            self_ty: self_ty.as_ref().map(|it| cx.ty(it)).unwrap_or_default(),
            self_matches,
            this: trait_.name(cx.db).as_str().to_owned(),
            trait_path: print::trait_path(cx, &tr),
            generics,
            crate_local,
        }
    }

    /// rustc's `OnUnimplementedCondition::matches_predicate`.
    fn holds(&self, cond: &Meta, direct: bool) -> bool {
        match cond {
            Meta::Word(word) => match word.as_str() {
                "crate_local" => self.crate_local,
                "direct" => direct,
                _ => false,
            },
            Meta::Value(name, value) => match name.as_str() {
                "Self" | "_Self" => {
                    let value = self.format(value);
                    self.self_matches.iter().any(|it| *it == value)
                }
                _ => self
                    .generics
                    .iter()
                    .any(|(param, arg)| param == name && *arg == self.format(value)),
            },
            Meta::List(name, inner) => match name.as_str() {
                "not" => inner.first().is_some_and(|it| !self.holds(it, direct)),
                "all" => inner.iter().all(|it| self.holds(it, direct)),
                "any" => inner.iter().any(|it| self.holds(it, direct)),
                _ => false,
            },
        }
    }

    /// rustc's `FormatString::format`: `{Self}`, `{This}`, `{Trait}` and the trait's generic
    /// names filled in, `{{` and `}}` as braces; the kinds `Self = ".."` compares with
    /// (`{union}`) are left as they are.
    fn format(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find(['{', '}']) {
            out.push_str(&rest[..at]);
            let tail = &rest[at..];
            if tail.starts_with("{{") || tail.starts_with("}}") {
                out.push_str(&tail[..1]);
                rest = &tail[2..];
                continue;
            }
            if tail.starts_with('}') {
                out.push('}');
                rest = &tail[1..];
                continue;
            }
            let Some(end) = tail.find('}') else {
                out.push_str(tail);
                rest = "";
                break;
            };
            let name = &tail[1..end];
            match name {
                "Self" => out.push_str(&self.self_ty),
                "This" => out.push_str(&self.this),
                "Trait" => out.push_str(&self.trait_path),
                _ => match self.generics.iter().find(|(param, _)| param == name) {
                    Some((_, arg)) => out.push_str(arg),
                    None => out.push_str(&tail[..=end]),
                },
            }
            rest = &tail[end + 1..];
        }
        out.push_str(rest);
        out
    }
}

/// An ADT's path with its crate's name, as rustc prints it with no trimmed paths.
fn full_path(cx: &Cx<'_, '_>, adt: hir::Adt) -> Option<String> {
    let krate = adt.module(cx.db).krate(cx.db).display_name(cx.db)?;
    let path = hir::ModuleDef::Adt(adt).canonical_path(cx.db, cx.edition)?;
    Some(format!("{krate}::{path}"))
}

/// The name rustc's `get_diagnostic_name` gives a trait it can derive: core's lang and
/// diagnostic items, or a trait marked `#[rustc_diagnostic_item = ".."]`.
fn derivable_name(cx: &Cx<'_, '_>, trait_: hir::Trait) -> Option<&'static str> {
    const DERIVABLE: [(LangItem, &str); 9] = [
        (LangItem::Copy, "Copy"),
        (LangItem::Clone, "Clone"),
        (LangItem::PartialEq, "PartialEq"),
        (LangItem::PartialOrd, "PartialOrd"),
        (LangItem::Eq, "Eq"),
        (LangItem::Ord, "Ord"),
        (LangItem::Hash, "Hash"),
        (LangItem::Debug, "Debug"),
        (LangItem::Default, "Default"),
    ];
    if let Some((_, name)) = DERIVABLE.iter().find(|(item, _)| is_lang(cx, trait_, *item)) {
        return Some(name);
    }
    let src = cx.sema.source(trait_)?;
    let named = src.value.attrs().find_map(|attr| match attr.meta()? {
        ast::Meta::KeyValueMeta(meta)
            if meta.path()?.syntax().text() == "rustc_diagnostic_item" =>
        {
            let ast::Expr::Literal(lit) = meta.expr()? else { return None };
            let ast::LiteralKind::String(s) = lit.kind() else { return None };
            Some(s.value().ok()?.into_owned())
        }
        _ => None,
    })?;
    DERIVABLE.iter().map(|(_, name)| *name).find(|name| *name == named)
}

/// rustc's `can_suggest_derive`: the self type is a local ADT, the trait one it can derive,
/// and every field implements it.
fn can_suggest_derive(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>) -> bool {
    if pred.polarity() == hir::PredicatePolarity::Negative {
        return false;
    }
    let tr = pred.trait_ref();
    let trait_ = tr.trait_();
    let Some(name) = derivable_name(cx, trait_) else { return false };
    let Some(self_ty) = tr.get_type_argument(0) else { return false };
    let Some(adt) = self_ty.as_adt() else { return false };
    if !is_local_adt(cx, adt) {
        return false;
    }
    let derivable = match name {
        "Copy" | "Clone" => true,
        _ if matches!(adt, hir::Adt::Union(_)) => false,
        "PartialEq" | "PartialOrd" => {
            tr.get_type_argument(1).is_some_and(|rhs| cx.ty(&rhs) == cx.ty(&self_ty))
        }
        _ => true,
    };
    if !derivable {
        return false;
    }
    let args: Vec<hir::Type<'_>> = self_ty.type_arguments().collect();
    let fields: Vec<hir::Field> = match adt {
        hir::Adt::Struct(it) => it.fields(cx.db),
        hir::Adt::Union(it) => it.fields(cx.db),
        hir::Adt::Enum(it) => it.variants(cx.db).into_iter().flat_map(|v| v.fields(cx.db)).collect(),
    };
    fields.iter().all(|field| {
        let ty = field.ty(cx.db).instantiate_type_params(args.iter());
        let rhs: Vec<hir::Type<'_>> = match name {
            "PartialEq" | "PartialOrd" => vec![ty.clone()],
            _ => Vec::new(),
        };
        ty.impls_trait(cx.db, trait_, &rhs)
    })
}

/// rustc's `suggest_derive`: "consider annotating `S` with `#[derive(Trait)]`".
fn suggest_derive(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>, diag: &mut Diag) {
    let tr = pred.trait_ref();
    let Some(name) = derivable_name(cx, tr.trait_()) else { return };
    let Some(self_ty) = tr.get_type_argument(0) else { return };
    let Some(adt) = self_ty.as_adt() else { return };
    let Some(at) = adt_def_span(cx, adt) else { return };
    diag.suggestion_verbose(
        at.shrink_to_lo(),
        format!("consider annotating `{}` with `#[derive({name})]`", cx.ty(&self_ty)),
        format!("#[derive({name})]\n"),
        Applicability::MaybeIncorrect,
    );
}

/// The span rustc's `def_span` gives an ADT: its header.
fn adt_def_span(cx: &Cx<'_, '_>, adt: hir::Adt) -> Option<Span> {
    let src = cx.sema.source(adt)?;
    Some(cx.span(src.as_ref().map(|it| def_range(it.syntax()))))
}

fn is_local_adt(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    adt.module(cx.db).krate(cx.db) == cx.krate
}

fn is_local_trait(cx: &Cx<'_, '_>, trait_: hir::Trait) -> bool {
    trait_.module(cx.db).krate(cx.db) == cx.krate
}

pub(super) fn is_lang(cx: &Cx<'_, '_>, trait_: hir::Trait, item: LangItem) -> bool {
    hir::Trait::lang(cx.db, cx.krate, item) == Some(trait_)
}

/// Whether the trait is `#[unstable]`: rustc then calls it "nightly-only, unstable".
fn is_unstable(cx: &Cx<'_, '_>, trait_: hir::Trait) -> bool {
    let Some(src) = cx.sema.source(trait_) else { return false };
    src.value.attrs().any(|attr| attr.path().is_some_and(|it| it.syntax().text() == "unstable"))
}
