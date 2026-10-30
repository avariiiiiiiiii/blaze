//! The errors of rustc's AST validation: what it rejects of the crate once it is expanded and
//! `cfg` has removed what it removes, before names are resolved. The lints it buffers for the
//! early lint pass (`missing_abi`, `patterns_in_fns_without_body`, ..) are
//! `crate::lints::syntax::early::validation`'s; what hir reports of the same thing
//! (`MissingBody`, `PatternArgInExternFn`, `TraitImplIncorrectSafety`) is lowered in
//! `crate::lower` and given here no second time.
//!
//! - `items`: `visit_item`, `visit_foreign_item`, `visit_assoc_item` (visibilities, `default`,
//!   bodies, safety, auto traits, `extern` blocks, `#[no_mangle]` and module names), and
//!   the parser's check of the modifiers of an inherent impl.
//! - `fns`: `visit_fn` (parameters, the ABI's signature, `#[track_caller]`, C-variadics,
//!   `const` with `async`, patterns of a function without a body) of functions and closures.
//! - `generics`: `visit_generics`, `visit_param_bound`, `visit_generic_args` and the
//!   `[const]` bounds a context allows.
//! - `types`: `visit_ty_common` (function pointers, trait objects, `impl Trait`).
//!
//! Each error is placed where rustc's walk comes to its node ([`Walk`]), so they come in the
//! order the validator gives them: an item's own checks, then what is in it, the errors of
//! leaving an `impl Trait` after everything in it.
//!
//! Follows rustc's `compiler/rustc_ast_passes/src/ast_validation.rs` (`AstValidator`,
//! `check_crate`) and `diagnostics.rs`, `compiler/rustc_parse/src/parser/item.rs`
//! (`parse_item_impl`: `TraitImplModifierInInherentImpl`), `compiler/rustc_span/src/source_map.rs`
//! (`guess_head_span`, `span_until_char`, `span_extend_while_whitespace`,
//! `span_until_non_whitespace`, `end_point`) and `compiler/rustc_session/src/parse.rs`
//! (`feature_err`) (MIT or Apache-2.0).

mod fns;
mod generics;
mod items;
mod types;

use std::{cell::OnceCell, ops::ControlFlow};

use hir::{HirFileId, InFile};
use span::{Edition, SyntaxContext};
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxNodePtr, SyntaxToken, TextRange, TextSize,
    ast,
};

use crate::{
    cx::{Cx, item_range},
    lints::syntax::{Pass, Tree, Walk, source_fact},
    lower::{Found, Phase, mutable_ref_binding::BUILT_ON},
    model::{Diag, Span},
};

/// One pass over the nodes [`Walk`] holds: each node costs a `kind` read and a `match` on it;
/// only the kinds a check looks at are cast, and nothing is allocated unless an error fires.
pub(crate) fn check(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Found> {
    let features = OnceCell::new();
    let mut out = Out::default();
    for tree in &walk.trees {
        let v = V { cx, walk, tree, features: &features };
        for node in &tree.nodes {
            v.visit(node, &mut out);
        }
    }
    out.finish(cx, walk)
}

/// The errors found, each with the node rustc's walk gives it at and whether it is given as
/// the walk leaves that node.
#[derive(Default)]
struct Out {
    found: Vec<(InFile<SyntaxNodePtr>, Pass, Found)>,
}

impl Out {
    fn finish(mut self, cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Found> {
        if self.found.is_empty() {
            return Vec::new();
        }
        self.found.sort_by_cached_key(|(at, pass, _)| walk.place(cx, *at, *pass));
        self.found.into_iter().map(|(_, _, found)| found).collect()
    }
}

/// The validator at one tree of the walk.
struct V<'v, 'a, 'db, 'w> {
    cx: &'v Cx<'a, 'db>,
    walk: &'v Walk<'w>,
    tree: &'v Tree,
    /// The crate's `#![feature]`s, read the first time a check asks.
    features: &'v OnceCell<Vec<String>>,
}

impl V<'_, '_, '_, '_> {
    /// The checks rustc's validator makes as it comes to `node`, in its order.
    fn visit(&self, node: &SyntaxNode, out: &mut Out) {
        match node.kind() {
            SyntaxKind::FN => items::fn_(self, node, out),
            SyntaxKind::IMPL => items::impl_(self, node, out),
            SyntaxKind::TRAIT => items::trait_(self, node, out),
            SyntaxKind::EXTERN_BLOCK => items::extern_block(self, node, out),
            SyntaxKind::ENUM => items::enum_(self, node, out),
            SyntaxKind::MODULE => items::module(self, node, out),
            SyntaxKind::STRUCT => items::struct_(self, node, out),
            SyntaxKind::UNION => items::union_(self, node, out),
            SyntaxKind::CONST => items::const_(self, node, out),
            SyntaxKind::STATIC => items::static_(self, node, out),
            SyntaxKind::TYPE_ALIAS => items::type_alias(self, node, out),
            SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF => items::no_mangle(self, node, out),
            SyntaxKind::CLOSURE_EXPR => fns::closure(self, node, out),
            SyntaxKind::GENERIC_PARAM_LIST => generics::params(self, node, out),
            SyntaxKind::WHERE_PRED => generics::where_pred(self, node, out),
            SyntaxKind::TYPE_BOUND => generics::bound(self, node, out),
            SyntaxKind::GENERIC_ARG_LIST => generics::args(self, node, out),
            SyntaxKind::FN_PTR_TYPE => types::fn_ptr(self, node, out),
            SyntaxKind::DYN_TRAIT_TYPE => types::dyn_trait(self, node, out),
            SyntaxKind::IMPL_TRAIT_TYPE => types::impl_trait(self, node, out),
            _ => {}
        }
    }

    /// The span rustc gives `range` of this tree.
    fn span(&self, range: TextRange) -> Span {
        self.cx.span(InFile::new(self.tree.file, range))
    }

    /// Gives `diag`, found at `node`, as the walk comes to it.
    fn emit(&self, out: &mut Out, node: &SyntaxNode, diag: Diag) {
        self.push(out, node, diag, Phase::Resolve, Pass::Early);
    }

    /// Gives `diag`, found at `node`, as the walk leaves it.
    fn emit_after(&self, out: &mut Out, node: &SyntaxNode, diag: Diag) {
        self.push(out, node, diag, Phase::Resolve, Pass::Post);
    }

    /// Gives `diag`, an error rustc's parser gives of `node`.
    fn emit_parse(&self, out: &mut Out, node: &SyntaxNode, diag: Diag) {
        self.push(out, node, diag, Phase::Parse, Pass::Early);
    }

    fn push(&self, out: &mut Out, node: &SyntaxNode, mut diag: Diag, phase: Phase, pass: Pass) {
        let at = self.tree.ptr(node);
        source_fact(self.cx, &mut diag, at);
        out.found.push((at, pass, Found::new(diag, phase)));
    }

    /// rustc's `Span::edition` of the token at `offset` of this tree.
    fn edition_at(&self, offset: TextSize) -> Edition {
        let file = self.tree.file;
        let ctxt = match file.macro_file() {
            None => SyntaxContext::root(file.edition(self.cx.db)),
            Some(call) => call.expansion_span_map(self.cx.db).span_at(offset).ctx,
        };
        ctxt.edition(self.cx.db)
    }

    /// The crate's language and library features, as its `#![feature(..)]`s enable them.
    fn feature(&self, name: &str) -> bool {
        let features = self.features.get_or_init(|| {
            let mut features = Vec::new();
            for meta in crate::checks::crate_attrs(self.cx) {
                let ast::Meta::TokenTreeMeta(meta) = meta else { continue };
                let is_feature = meta
                    .path()
                    .and_then(|it| it.as_single_name_ref())
                    .is_some_and(|it| it.text() == "feature");
                let Some(tt) = meta.token_tree().filter(|_| is_feature) else { continue };
                for token in tt.syntax().children_with_tokens().filter_map(NodeOrToken::into_token) {
                    if token.kind() == SyntaxKind::IDENT {
                        features.push(unraw(token.text()).to_owned());
                    }
                }
            }
            features
        });
        features.iter().any(|it| it == name)
    }

    /// The `target_arch` of the crate's target.
    fn arch(&self) -> Option<&str> {
        self.walk.cfg.get_cfg_values("target_arch").next().map(|it| it.as_str())
    }

    /// Whether `cfg` keeps `node` (a variant or a field of an item the walk came to).
    fn kept(&self, node: &SyntaxNode) -> bool {
        !self.tree.skipped(node.text_range())
    }

    /// Where rustc's span of `node`'s attribute `name` is (`#[name]`, or the attribute in a
    /// `cfg_attr` that gives it), once `cfg_attr` is expanded.
    fn attr(&self, node: &SyntaxNode, name: &str) -> Option<TextRange> {
        let owner = ast::AnyHasAttrs::cast(node.clone())?;
        let cfg = self.walk.cfg;
        hir_expand::attrs::expand_cfg_attr(ast::attrs_including_inner(&owner), || cfg, |meta, attr| {
            if is_named(&meta, name) { ControlFlow::Break(attr_range(&meta, &attr)) } else { ControlFlow::Continue(()) }
        })
    }

    fn has_attr(&self, node: &SyntaxNode, name: &str) -> bool {
        self.attr(node, name).is_some()
    }

    /// Where rustc's validator is at `node`: the trait, impl or `extern` block whose item it
    /// is, else free. An item of an expansion is where the macro call is.
    fn ctxt(&self, node: &SyntaxNode) -> Ctxt {
        for at in self.ancestors(node) {
            match at.value.kind() {
                SyntaxKind::ASSOC_ITEM_LIST => {
                    let Some(parent) = at.value.parent() else { return Ctxt::Free };
                    if let Some(it) = ast::Trait::cast(parent.clone()) {
                        return Ctxt::Trait(at.with_value(it));
                    }
                    if let Some(it) = ast::Impl::cast(parent) {
                        return match it.trait_() {
                            Some(_) => Ctxt::TraitImpl(at.with_value(it)),
                            None => Ctxt::Impl(at.with_value(it)),
                        };
                    }
                    return Ctxt::Free;
                }
                SyntaxKind::EXTERN_ITEM_LIST => {
                    return match at.value.parent().and_then(ast::ExternBlock::cast) {
                        Some(it) => Ctxt::Foreign(at.with_value(it)),
                        None => Ctxt::Free,
                    };
                }
                SyntaxKind::ITEM_LIST | SyntaxKind::SOURCE_FILE | SyntaxKind::STMT_LIST => return Ctxt::Free,
                _ => {}
            }
        }
        Ctxt::Free
    }

    /// The nodes `node` is in, innermost first, through the macro calls of the expansions it
    /// is in: a function-like call's node, then what it is in; an attribute macro's or a
    /// derive's, what the item it replaces or is on is in.
    fn ancestors(&self, node: &SyntaxNode) -> Ancestors<'_> {
        Ancestors { db: self.cx.db, file: self.tree.file, next: node.parent() }
    }
}

/// The nodes a node is in (see [`V::ancestors`]).
struct Ancestors<'db> {
    db: &'db dyn hir::db::HirDatabase,
    file: HirFileId,
    next: Option<SyntaxNode>,
}

impl Iterator for Ancestors<'_> {
    type Item = InFile<SyntaxNode>;

    fn next(&mut self) -> Option<InFile<SyntaxNode>> {
        loop {
            if let Some(node) = self.next.take() {
                self.next = node.parent();
                return Some(InFile::new(self.file, node));
            }
            let call = self.file.macro_file()?;
            let at = call.loc(self.db).to_node(self.db);
            self.file = at.file_id;
            self.next = match at.value.kind() {
                SyntaxKind::MACRO_CALL => Some(at.value),
                SyntaxKind::ATTR => at.value.parent().and_then(|it| it.parent()),
                _ => at.value.parent(),
            };
        }
    }
}

/// What rustc's validator knows of the item it is at (`outer_trait_or_trait_impl`,
/// `extern_mod_span`): the trait, impl or `extern` block it is in.
enum Ctxt {
    Free,
    Foreign(InFile<ast::ExternBlock>),
    Trait(InFile<ast::Trait>),
    TraitImpl(InFile<ast::Impl>),
    Impl(InFile<ast::Impl>),
}

impl Ctxt {
    /// `TraitOrImpl::constness`: the `const` of the trait or impl it is in.
    fn constness(&self) -> Option<InFile<SyntaxToken>> {
        match self {
            Ctxt::Trait(it) => it.as_ref().map(|it| const_token(it.syntax())).transpose(),
            Ctxt::TraitImpl(it) | Ctxt::Impl(it) => it.as_ref().map(|it| it.const_token()).transpose(),
            Ctxt::Free | Ctxt::Foreign(_) => None,
        }
    }

    fn is_assoc(&self) -> bool {
        matches!(self, Ctxt::Trait(_) | Ctxt::TraitImpl(_) | Ctxt::Impl(_))
    }
}

/// An item's `const` before its keyword (`const trait`, `const impl`).
fn const_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .take_while(|it| !matches!(it.kind(), SyntaxKind::TRAIT_KW | SyntaxKind::IMPL_KW))
        .find(|it| it.kind() == SyntaxKind::CONST_KW)
}

/// Whether `meta`'s path is the one segment `name`.
fn is_named(meta: &ast::Meta, name: &str) -> bool {
    !matches!(meta, ast::Meta::CfgMeta(_) | ast::Meta::CfgAttrMeta(_))
        && meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == name)
}

/// rustc's `attr_span` of `meta`, written in `attr`: the whole `#[..]`, or for one a
/// `cfg_attr` gives, the attribute in it (with its `unsafe(..)`).
fn attr_range(meta: &ast::Meta, attr: &ast::Attr) -> TextRange {
    let inner = meta.syntax().parent().filter(|it| it.kind() == SyntaxKind::UNSAFE_META);
    let inner = inner.as_ref().unwrap_or(meta.syntax());
    match inner.parent() {
        Some(parent) if &parent == attr.syntax() => attr.syntax().text_range(),
        _ => inner.text_range(),
    }
}

/// An identifier's name, without the `r#` of a raw one.
fn unraw(text: &str) -> &str {
    text.strip_prefix("r#").unwrap_or(text)
}

/// The text of `range` of the tree `node` is in (`range` within `node`).
fn text_of(node: &SyntaxNode, range: TextRange) -> String {
    let text = node.to_string();
    let base = node.text_range().start();
    text[usize::from(range.start() - base)..usize::from(range.end() - base)].to_owned()
}

/// rustc's `guess_head_span` of an item's span (`item_range`): up to its first `{`, the white
/// space before it trimmed, when that is not empty and is on one line; else the whole span.
fn head_range(node: &SyntaxNode) -> TextRange {
    let range = item_range(node);
    let text = text_of(node, range);
    let head = text.split('{').next().unwrap_or("").trim_end();
    if head.is_empty() || head.contains('\n') {
        return range;
    }
    TextRange::at(range.start(), TextSize::of(head))
}

/// rustc's `ending_semi_or_hi` of an item's span: its last character when that is `;`, else
/// its end.
fn ending_semi_or_hi(node: &SyntaxNode) -> TextRange {
    let range = item_range(node);
    let last = node.last_token().filter(|it| it.kind() == SyntaxKind::SEMICOLON);
    match last {
        Some(semi) if semi.text_range().end() == range.end() => semi.text_range(),
        _ => TextRange::empty(range.end()),
    }
}

/// rustc's `span_extend_while_whitespace` of a token: it and the white space after it.
fn extend_while_whitespace(token: &SyntaxToken) -> TextRange {
    let range = token.text_range();
    match token.next_token() {
        Some(next) if next.kind() == SyntaxKind::WHITESPACE => range.cover(next.text_range()),
        _ => range,
    }
}

/// rustc's `span_until_non_whitespace` of the span from `token` to `end`: `token` and the
/// white space after it.
fn until_non_whitespace(token: &SyntaxToken, end: TextSize) -> TextRange {
    let range = extend_while_whitespace(token);
    TextRange::new(range.start(), range.end().min(end.max(range.start())))
}

/// rustc's `feature_err` of `feature` on a nightly compiler: E0658 with the feature's issue
/// and how to enable it.
fn feature_err(feature: &str, issue: Option<u32>, message: impl Into<String>, span: Span) -> Diag {
    let mut diag = Diag::error(Some("E0658"), message, span);
    if let Some(issue) = issue {
        diag.note(format!("see issue #{issue} <https://github.com/rust-lang/rust/issues/{issue}> for more information"));
    }
    diag.help(format!("add `#![feature({feature})]` to the crate attributes to enable"));
    diag.note(format!("this compiler was built on {BUILT_ON}; consider upgrading it if it is out of date"));
    diag
}
