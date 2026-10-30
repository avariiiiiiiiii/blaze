//! E0133: an unsafe operation outside an `unsafe` block or function; and its two lints,
//! `unsafe_op_in_unsafe_fn` (one in the body of an `unsafe fn`, outside a block) and
//! `deprecated_safe_2024` (a call of a function that is unsafe only from edition 2024, before
//! it).
//!
//! Follows rustc's `compiler/rustc_mir_build/src/check_unsafety.rs`
//! (`UnsafetyVisitor::requires_unsafe`, `emit_deprecated_safe_fn_call`, `visit_inner_body`,
//! `UnsafeOpKind::emit_unsafe_op_in_unsafe_fn_lint` and `emit_requires_unsafe_err`) and
//! `compiler/rustc_mir_build/src/diagnostics.rs` (the `*RequiresUnsafe*` diagnostics,
//! `UnsafeNotInheritedNote`, `UnsafeNotInheritedLintNote`,
//! `CallToDeprecatedSafeFnRequiresUnsafe`), with `compiler/rustc_span/src/source_map.rs`
//! (`guess_head_span`, `indentation_before`) (MIT or Apache-2.0).

use std::cmp::Reverse;

use blaze_items::{Fact, ItemId};
use cfg::CfgAtom;
use hir::{
    AsAssocItem as _, AssocItemContainer, CallableKind, HasSource as _, HirFileId, InFile, ItemInNs,
    ModuleDef, PathResolution, UnsafeLint, UnsafetyReason,
};
use hir_def::{ItemContainerId, Lookup as _, attrs::AttrFlags, signatures::FunctionSignature};
use intern::{Symbol, sym};
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, TextSize,
    ast::{self, HasAttrs as _},
};

use crate::{
    cx::{Cx, item_range, signature_range},
    lints::{body_item, code_fact},
    lower::{Found, Phase, break_outside_of_loop::ancestors},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MissingUnsafe) -> Vec<Found> {
    let ptr = d.node.map(|it| it.syntax_node_ptr());
    let node = cx.syntax(ptr);
    let span = cx.span_of(ptr);
    let op = Op::of(cx, d.reason, &node, ptr);
    let found = match d.lint {
        UnsafeLint::HardError => Found::new(requires_unsafe(cx, &op, &node, span, ptr), Phase::Mir),
        UnsafeLint::UnsafeOpInUnsafeFn => {
            let mut diag = op.diag(cx, Diag::lint, "unsafe_op_in_unsafe_fn", "block", true, span);
            unsafe_fn_note(cx, &mut diag, &node);
            op.facts(cx, &mut diag);
            code_fact(cx, &mut diag, ptr);
            Found::lint(diag, Phase::Mir, lint_at(cx, &node))
        }
        UnsafeLint::DeprecatedSafe2024 => match deprecated_safe(cx, &op, span.clone()) {
            Some(mut diag) => {
                op.facts(cx, &mut diag);
                code_fact(cx, &mut diag, ptr);
                Found::lint(diag, Phase::Mir, lint_at(cx, &node))
            }
            // rustc lints a call only when the function's attribute says what to audit, else
            // the call is the error of any call of an unsafe function.
            None => Found::new(requires_unsafe(cx, &op, &node, span, ptr), Phase::Mir),
        },
    };
    vec![found]
}

/// rustc's `UnsafeOpKind`, of the operations hir finds.
enum Op {
    /// A call of an unsafe function, which is named unless it is called through a pointer.
    Call(Option<hir::Function>),
    /// A call of a safe function with `#[target_feature]` from a body without its features:
    /// those missing, in the order the callee names them, and those of them the build enables.
    CallWithFeatures { function: hir::Function, missing: Vec<Symbol>, build_enabled: Vec<Symbol> },
    InlineAsm,
    MutableStatic(Option<hir::Static>),
    ExternStatic(Option<hir::Static>),
    RawPtrDeref,
    UnionField(Option<hir::Union>),
}

impl Op {
    fn of(cx: &Cx<'_, '_>, reason: UnsafetyReason, node: &InFile<SyntaxNode>, at: InFile<SyntaxNodePtr>) -> Op {
        match reason {
            UnsafetyReason::UnsafeFnCall => call(cx, node, at),
            UnsafetyReason::InlineAsm => Op::InlineAsm,
            UnsafetyReason::RawPtrDeref => Op::RawPtrDeref,
            UnsafetyReason::MutableStatic => Op::MutableStatic(static_of(cx, node)),
            UnsafetyReason::ExternStatic => Op::ExternStatic(static_of(cx, node)),
            UnsafetyReason::UnionField => Op::UnionField(union_of(cx, node)),
        }
    }

    /// The diagnostic of the operation, made by `make` (an error or a lint named `name`), that
    /// it "requires unsafe {needs}", the callee named by its whole path when `untrimmed`.
    fn diag(
        &self,
        cx: &Cx<'_, '_>,
        make: fn(&str, String, Span) -> Diag,
        name: &str,
        needs: &str,
        untrimmed: bool,
        span: Span,
    ) -> Diag {
        let path = |function: hir::Function| match untrimmed {
            true => untrimmed_path(cx, function),
            false => cx.def_path(ModuleDef::Function(function)),
        };
        let (what, label) = match self {
            Op::Call(Some(function)) => (format!("call to unsafe function `{}`", path(*function)), "call to unsafe function"),
            Op::Call(None) => ("call to unsafe function".to_owned(), "call to unsafe function"),
            Op::CallWithFeatures { function, .. } => (
                format!("call to function `{}` with `#[target_feature]`", path(*function)),
                "call to function with `#[target_feature]`",
            ),
            Op::InlineAsm => ("use of inline assembly".to_owned(), "use of inline assembly"),
            Op::MutableStatic(_) => ("use of mutable static".to_owned(), "use of mutable static"),
            Op::ExternStatic(_) => ("use of extern static".to_owned(), "use of extern static"),
            Op::RawPtrDeref => ("dereference of raw pointer".to_owned(), "dereference of raw pointer"),
            Op::UnionField(_) => ("access to union field".to_owned(), "access to union field"),
        };
        let mut diag = make(name, format!("{what} is unsafe and requires unsafe {needs}"), span.clone());
        diag.span_label(span, label);
        match self {
            Op::CallWithFeatures { missing, build_enabled, .. } => {
                let features = match missing.len() {
                    1 => "feature",
                    _ => "features",
                };
                diag.help(format!(
                    "in order for the call to be safe, the context requires the following additional target {features}: {}",
                    and_list(missing)
                ));
                if !build_enabled.is_empty() {
                    let (features, them) = match build_enabled.len() {
                        1 => ("feature", "it"),
                        _ => ("features", "them"),
                    };
                    diag.note(format!(
                        "the {} target {features} being enabled in the build configuration does not remove the requirement to list {them} in `#[target_feature]`",
                        and_list(build_enabled)
                    ));
                }
            }
            _ => {
                diag.note(self.note());
            }
        }
        diag
    }

    /// What rustc's note says the operation risks.
    fn note(&self) -> &'static str {
        match self {
            Op::Call(_) | Op::CallWithFeatures { .. } => {
                "consult the function's documentation for information on how to avoid undefined behavior"
            }
            Op::InlineAsm => "inline assembly is entirely unchecked and can cause undefined behavior",
            Op::MutableStatic(_) => {
                "mutable statics can be mutated by multiple threads: aliasing violations or data races will cause undefined behavior"
            }
            Op::ExternStatic(_) => {
                "extern statics are not controlled by the Rust type system: invalid data, aliasing violations or data races will cause undefined behavior"
            }
            Op::RawPtrDeref => {
                "raw pointers may be null, dangling or unaligned; they can violate aliasing rules and cause data races: all of these are undefined behavior"
            }
            Op::UnionField(_) => {
                "the field may not be properly initialized: using uninitialized data will cause undefined behavior"
            }
        }
    }

    /// Records what made the operation unsafe: the callee's signature, the static's, the
    /// union's definition.
    fn facts(&self, cx: &Cx<'_, '_>, diag: &mut Diag) {
        match self {
            Op::Call(Some(function)) | Op::CallWithFeatures { function, .. } => {
                if let Ok(id) = hir_def::FunctionId::try_from(*function) {
                    let path = cx.def_path(ModuleDef::Function(*function));
                    diag.fact(Fact::Signature(cx.key(ItemId::Function(id))), format!("the signature of `{path}`"));
                }
            }
            Op::MutableStatic(Some(it)) | Op::ExternStatic(Some(it)) => {
                let path = cx.def_path(ModuleDef::Static(*it));
                diag.fact(Fact::Signature(cx.key(ItemId::Static((*it).into()))), format!("the signature of `{path}`"));
            }
            Op::UnionField(Some(it)) => {
                let adt = hir::Adt::Union(*it);
                let path = cx.def_path(ModuleDef::Adt(adt));
                diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("the definition of `{path}`"));
            }
            _ => {}
        }
    }
}

/// rustc's `emit_requires_unsafe_err`: the error, and the label on the `unsafe` block or
/// function around the item the operation is in, which it does not inherit.
fn requires_unsafe(cx: &Cx<'_, '_>, op: &Op, node: &InFile<SyntaxNode>, span: Span, at: InFile<SyntaxNodePtr>) -> Diag {
    let needs = match op_in_unsafe_fn_allowed(cx) {
        true => "function or block",
        false => "block",
    };
    let error = |_: &str, message: String, span: Span| Diag::error(Some("E0133"), message, span);
    let mut diag = op.diag(cx, error, "", needs, false, span);
    if let Some(head) = not_inherited(cx, node) {
        diag.span_label(head, "items do not inherit unsafety from separate enclosing items");
    }
    op.facts(cx, &mut diag);
    code_fact(cx, &mut diag, at);
    diag
}

/// Whether `unsafe_op_in_unsafe_fn` is `allow` where the operation is, which makes rustc's
/// error say "requires unsafe function or block": its default level, `allow` before edition
/// 2024. An attribute or flag that sets it (`#![deny(unsafe_op_in_unsafe_fn)]`) needs the
/// level at a node from the levels (v2/requests/diagnostics.md, diagnostics-lints-mir-unsafe).
fn op_in_unsafe_fn_allowed(cx: &Cx<'_, '_>) -> bool {
    !cx.edition.at_least_2024()
}

/// The head (`guess_head_span`) of the first `unsafe` block or `unsafe fn` around `node`'s
/// item: rustc's `UnsafeNotInheritedNote`. Closures and inline consts are checked with their
/// body, so one in a block is never in error.
fn not_inherited(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<Span> {
    ancestors(cx, node).into_iter().skip(1).find_map(|it| {
        let unsafe_ = match_ast_unsafe(&it.value)?;
        let range = item_range(&it.value);
        Some(cx.span(it.with_value(head_range(&it.value, range, unsafe_))))
    })
}

/// Whether `node` is an `unsafe` block or an `unsafe fn`, and so where its head starts.
fn match_ast_unsafe(node: &SyntaxNode) -> Option<()> {
    if let Some(block) = ast::BlockExpr::cast(node.clone()) {
        return block.unsafe_token().map(drop);
    }
    ast::Fn::cast(node.clone())?.unsafe_token().map(drop)
}

/// rustc's `guess_head_span` of `range` of `node`: up to its first `{`, without the white
/// space before it, when that is on one line and not empty.
fn head_range(node: &SyntaxNode, range: TextRange, _: ()) -> TextRange {
    let text = node.text();
    let base = node.text_range().start();
    let local = TextRange::new(range.start() - base, range.end() - base);
    let written = text.slice(local).to_string();
    let head = written.split('{').next().unwrap_or("").trim_end();
    match !head.is_empty() && !head.contains('\n') {
        true => TextRange::at(range.start(), TextSize::of(head)),
        false => range,
    }
}

/// rustc's `UnsafeNotInheritedLintNote` on the first operation it lints in the function (and
/// in each closure and inline const it, from what it was where that starts): the function's
/// signature, and the tool-only suggestion to wrap its body in an `unsafe` block.
fn unsafe_fn_note(cx: &Cx<'_, '_>, diag: &mut Diag, node: &InFile<SyntaxNode>) {
    let Some(function) = ancestors(cx, node).into_iter().find_map(|it| it.map(ast::Fn::cast).transpose()) else {
        return;
    };
    if function.value.unsafe_token().is_none() || !first_in_function(cx, node, &function) {
        return;
    }
    let signature = cx.span(function.with_value(signature_range(&function.value)));
    diag.span_note(signature, "an unsafe function restricts its caller, but its body is safe by default");
    if let Some(body) = function.value.body() {
        let body = cx.span(function.with_value(body.syntax().text_range()));
        diag.suggestion_with_style(
            vec![(body.shrink_to_lo(), "{ unsafe ".to_owned()), (body.shrink_to_hi(), "}".to_owned())],
            "consider wrapping the function body in an unsafe block",
            Applicability::MachineApplicable,
            SuggestionStyle::CompletelyHidden,
        );
    }
}

/// Whether rustc's `suggest_unsafe_block` is still set at the operation `node`: no operation
/// outside an `unsafe` block comes before it in its closure (or inline const), or before that
/// in what that is in, up to `function`. rustc checks a closure with a copy of the flag, so
/// what is linted in one leaves the flag of what it is in set.
fn first_in_function(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>, function: &InFile<ast::Fn>) -> bool {
    let Some(def) = cx.sema.to_def2(function.as_ref()) else { return true };
    let Some(scopes) = scopes(cx, node) else { return true };
    let position = position(cx, node);
    let own = key(node);
    let ops = cx.sema.get_unsafe_ops(hir::DefWithBody::Function(def).into());
    !ops.iter().any(|other| {
        let other = cx.syntax(other.map(|it| it.syntax_node_ptr()));
        if key(&other) == own {
            return false;
        }
        let Some(other_scopes) = scopes(cx, &other) else { return false };
        other_scopes.first().is_some_and(|scope| scopes.contains(scope)) && self::position(cx, &other) < position
    })
}

/// Where a node is, to tell two apart across the trees of a file and its expansions.
fn key(node: &InFile<SyntaxNode>) -> (HirFileId, TextRange) {
    (node.file_id, node.value.text_range())
}

/// The bodies rustc checks `node` in, from its own: each closure and inline const around it,
/// then the function; none when it is in an `unsafe` block, where rustc lints nothing.
fn scopes(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<Vec<(HirFileId, TextRange)>> {
    let mut scopes = Vec::new();
    for it in ancestors(cx, node).into_iter().skip(1) {
        if let Some(block) = ast::BlockExpr::cast(it.value.clone()) {
            if block.unsafe_token().is_some() {
                return None;
            }
            if block.const_token().is_some() {
                scopes.push(key(&it));
            }
        } else if ast::ClosureExpr::can_cast(it.value.kind()) {
            scopes.push(key(&it));
        } else if ast::Fn::can_cast(it.value.kind()) {
            scopes.push(key(&it));
            return Some(scopes);
        }
    }
    Some(scopes)
}

/// The order rustc visits operations in: by where each starts in the file and in each
/// expansion down to it, one around another first.
fn position(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Vec<(TextSize, Reverse<TextSize>)> {
    let mut at = node.clone();
    let mut position = Vec::new();
    loop {
        let range = at.value.text_range();
        position.push((range.start(), Reverse(range.end())));
        let Some(call) = at.file_id.macro_file() else { break };
        at = call.loc(cx.db).to_node(cx.db);
    }
    position.reverse();
    position
}

/// rustc's `CallToDeprecatedSafeFnRequiresUnsafe`, when the callee's
/// `#[rustc_deprecated_safe_2024(audit_that = "..")]` says what to audit.
fn deprecated_safe(cx: &Cx<'_, '_>, op: &Op, span: Span) -> Option<Diag> {
    let Op::Call(Some(function)) = op else { return None };
    let audit = audit_that(cx, *function)?;
    let path = untrimmed_path(cx, *function);
    let message = format!("call to deprecated safe function `{path}` is unsafe and requires unsafe block");
    let mut diag = Diag::lint("deprecated_safe_2024", message, span.clone());
    diag.span_label(span.clone(), "call to unsafe function");
    let (line_start, indent) = line_of(cx, &span);
    diag.multipart_suggestion(
        format!("you can wrap the call in an `unsafe` block if you can guarantee that {audit}"),
        vec![
            (line_start, format!("{indent}// FIXME: Audit that {audit}.\n")),
            (span.shrink_to_lo(), "unsafe { ".to_owned()),
            (span.shrink_to_hi(), " }".to_owned()),
        ],
        Applicability::MachineApplicable,
    );
    Some(diag)
}

/// The start of the line `span` starts on, and the white space it starts with before `span`
/// (`span_extend_to_line`, `indentation_before`).
fn line_of(cx: &Cx<'_, '_>, span: &Span) -> (Span, String) {
    let Some(text) = cx.file_of(span).map(|it| cx.file_text(it)) else {
        return (span.shrink_to_lo(), String::new());
    };
    let before = &text[..span.lo as usize];
    let start = before.rfind('\n').map_or(0, |it| it + 1);
    let line = &before[start..];
    let indent = line.find(|c: char| !c.is_whitespace()).map_or(line, |it| &line[..it]);
    (Span { lo: start as u32, hi: start as u32, ..span.clone() }, indent.to_owned())
}

/// What `#[rustc_deprecated_safe_2024(audit_that = "..")]` on `function` says to audit.
fn audit_that(cx: &Cx<'_, '_>, function: hir::Function) -> Option<String> {
    let source = function.source(cx.db)?;
    source.value.attrs().find_map(|attr| {
        let (name, tt) = attr.as_simple_call()?;
        if name != "rustc_deprecated_safe_2024" {
            return None;
        }
        let mut tokens = tt
            .syntax()
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .filter(|it| !it.kind().is_trivia());
        while let Some(token) = tokens.next() {
            if token.kind() == SyntaxKind::IDENT && token.text() == "audit_that" {
                if tokens.next()?.kind() != SyntaxKind::EQ {
                    return None;
                }
                let value = ast::String::cast(tokens.next()?)?;
                return value.value().ok().map(|it| it.into_owned());
            }
        }
        None
    })
}

/// The operation of a call hir found unsafe: of the function it calls (an `unsafe fn`, one of
/// an `extern` block, one safe only before 2024), of one with `#[target_feature]`, or through
/// a pointer.
fn call(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>, at: InFile<SyntaxNodePtr>) -> Op {
    let function = match_callee(cx, &node.value);
    let Some(function) = function else { return Op::Call(None) };
    let Ok(id) = hir_def::FunctionId::try_from(function) else { return Op::Call(Some(function)) };
    let signature = FunctionSignature::of(cx.db, id);
    let extern_ = matches!(id.lookup(cx.db).container, ItemContainerId::ExternBlockId(_));
    let declared_unsafe = signature.is_unsafe() || signature.is_deprecated_safe_2024() || (extern_ && !signature.is_safe());
    if declared_unsafe || !signature.has_target_feature() {
        return Op::Call(Some(function));
    }
    let caller = match body_item(cx, at) {
        Some((ItemId::Function(caller), _)) => hir_ty::TargetFeatures::from_fn(cx.db, caller),
        _ => hir_ty::TargetFeatures::default(),
    };
    let missing: Vec<Symbol> = AttrFlags::target_feature_list(cx.db, id)
        .into_iter()
        .filter(|it| !caller.enables(it.as_str()))
        .collect();
    let cfg = cx.krate.cfg(cx.db);
    let build_enabled = missing
        .iter()
        .filter(|it| cfg.check_atom(&CfgAtom::KeyValue { key: sym::target_feature, value: (*it).clone() }))
        .cloned()
        .collect();
    Op::CallWithFeatures { function, missing, build_enabled }
}

/// The function a call or a method call calls, by the type of its callee: `None` through a
/// pointer.
fn match_callee(cx: &Cx<'_, '_>, node: &SyntaxNode) -> Option<hir::Function> {
    if let Some(call) = ast::MethodCallExpr::cast(node.clone()) {
        return cx.sema.resolve_method_call(&call);
    }
    let callee = ast::CallExpr::cast(node.clone())?.expr()?;
    match cx.sema.resolve_expr_as_callable(&callee)?.kind() {
        CallableKind::Function(it) => Some(it),
        _ => None,
    }
}

/// The static a path (an expression or the target of an assignment) names.
fn static_of(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<hir::Static> {
    let path = match ast::PathExpr::cast(node.value.clone()) {
        Some(it) => it.path()?,
        None => ast::PathPat::cast(node.value.clone())?.path()?,
    };
    match cx.sema.resolve_path(&path)? {
        PathResolution::Def(ModuleDef::Static(it)) => Some(it),
        _ => None,
    }
}

/// The union whose field a field expression reads, or a pattern of whose fields `node` is.
fn union_of(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<hir::Union> {
    if let Some(field) = ast::FieldExpr::cast(node.value.clone()) {
        return match cx.sema.resolve_field(&field)?.left()?.parent_def(cx.db) {
            hir::Variant::Union(it) => Some(it),
            _ => None,
        };
    }
    let record = node.value.ancestors().find_map(ast::RecordPat::cast)?;
    match cx.sema.resolve_path(&record.path()?)? {
        PathResolution::Def(ModuleDef::Adt(hir::Adt::Union(it))) => Some(it),
        _ => None,
    }
}

/// The node rustc emits the lint of an operation at (`hir_context`): the expression itself;
/// for a pattern, the expression it is in (a `let`'s block, a `match`, a closure), or the item
/// of a parameter.
fn lint_at(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> InFile<SyntaxNodePtr> {
    ancestors(cx, node)
        .into_iter()
        .find(|it| ast::Expr::can_cast(it.value.kind()) || ast::Item::can_cast(it.value.kind()))
        .unwrap_or_else(|| node.clone())
        .map(|it| SyntaxNodePtr::new(&it))
}

/// rustc's `def_path_str` under `with_no_trimmed_paths`, which its lints name callees with:
/// the path from the crate's root (`std::env::set_var`); of a method, its trait's or its
/// type's path, then the rest as rustc prints it.
fn untrimmed_path(cx: &Cx<'_, '_>, function: hir::Function) -> String {
    let trimmed = cx.def_path(ModuleDef::Function(function));
    match function.as_assoc_item(cx.db).map(|it| it.container(cx.db)) {
        None => full_path(cx, ItemInNs::Values(ModuleDef::Function(function))).unwrap_or(trimmed),
        Some(AssocItemContainer::Trait(trait_)) => {
            let trimmed_trait = cx.def_path(ModuleDef::Trait(trait_));
            match (full_path(cx, ItemInNs::Types(ModuleDef::Trait(trait_))), trimmed.strip_prefix(&trimmed_trait)) {
                (Some(full), Some(rest)) => format!("{full}{rest}"),
                _ => trimmed,
            }
        }
        Some(AssocItemContainer::Impl(impl_)) => {
            let Some(adt) = impl_.trait_(cx.db).is_none().then(|| impl_.self_ty(cx.db).as_adt()).flatten() else {
                return trimmed;
            };
            let trimmed_adt = cx.def_path(ModuleDef::Adt(adt));
            match (full_path(cx, ItemInNs::Types(ModuleDef::Adt(adt))), trimmed.strip_prefix(&trimmed_adt)) {
                (Some(full), Some(rest)) => format!("{full}{rest}"),
                _ => trimmed,
            }
        }
    }
}

/// The path of an item from the crate's root, as rustc's untrimmed paths name it, else where
/// it is defined.
fn full_path(cx: &Cx<'_, '_>, item: ItemInNs) -> Option<String> {
    let config = hir::FindPathConfig {
        prefer_no_std: false,
        prefer_prelude: false,
        prefer_absolute: false,
        allow_unstable: true,
    };
    let root = cx.krate.root_module(cx.db);
    if let Some(path) = root.find_path(cx.db, item, config) {
        return Some(path.display(cx.db, cx.edition).to_string());
    }
    let def = match item {
        ItemInNs::Types(it) | ItemInNs::Values(it) => it,
        ItemInNs::Macros(_) => return None,
    };
    def.canonical_path(cx.db, cx.edition)
}

/// rustc's `DiagArgValue::StrListSepByAnd` in English: `a`, `a and b`, `a, b, and c`.
fn and_list(items: &[Symbol]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_str().to_owned(),
        [a, b] => format!("{} and {}", a.as_str(), b.as_str()),
        [rest @ .., last] => {
            let mut out = String::new();
            for it in rest {
                out.push_str(it.as_str());
                out.push_str(", ");
            }
            out.push_str("and ");
            out.push_str(last.as_str());
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use intern::Symbol;

    use super::and_list;

    #[test]
    fn lists_are_joined_as_rustc_joins_them() {
        let list = |items: &[&str]| and_list(&items.iter().map(|it| Symbol::intern(it)).collect::<Vec<_>>());
        assert_eq!(list(&["avx"]), "avx");
        assert_eq!(list(&["avx", "bmi2"]), "avx and bmi2");
        assert_eq!(list(&["avx", "bmi2", "fma"]), "avx, bmi2, and fma");
    }
}
