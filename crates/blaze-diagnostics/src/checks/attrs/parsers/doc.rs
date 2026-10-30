//! The errors of the parsers of `#[doc(..)]` and of the diagnostic attributes
//! (`#[diagnostic::on_unimplemented]` and the rest, `#[rustc_on_unimplemented]`,
//! `#[rustc_diagnostic_opaque]`), and of `check_doc_attrs`.
//!
//! `#[doc]` is one parser of all of a node's `#[doc(..)]`: it gives one `Parsed::Doc` per node,
//! at its first `#[doc]`, when some list element was a meta item (rustc's `nb_doc_attrs`). A
//! `#[doc = "str"]` is the driver's and never reaches it. Every diagnostic of the diagnostic
//! attributes' parsers and of their `check_attr` checks is a lint, so they give only what they
//! parse; their `rustc_attrs` gates are `checks::features`'. The lints of `#[doc]` are
//! `lints::syntax::attrs::doc`'s.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/` `doc.rs`, `cfg.rs`
//! (`parse_cfg`, `parse_cfg_entry`) and `diagnostic/`, with the texts of
//! `compiler/rustc_attr_parsing/src/session_diagnostics.rs`, and `check_doc_attrs` of
//! `compiler/rustc_passes/src/check_attr.rs` with the texts of
//! `compiler/rustc_passes/src/diagnostics.rs` (MIT or Apache-2.0).

use std::mem;

use hir::InFile;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, TextRange,
    ast::{self, HasGenericArgs, HasGenericParams, HasModuleItem, HasName},
};

use super::{Accepted, Finalize, GroupId, parse_version};
use crate::{
    checks::{
        attrs::{
            accept::Accept,
            args::{ArgParser, Ident, MetaItem, MetaItemOrLit, MetaList, NameValue, Path},
            parsed::{Doc, DocInline, Parsed, ParsedAttr},
        },
        features::attributes::{cfg_entry, doc_rust_logo},
    },
    cx::Cx,
    lints::syntax::attrs::{Target, table},
    model::{Diag, Level, MultiSpan, Span},
};

pub(crate) const PATHS: &[&[&str]] = &[
    &["diagnostic", "do_not_recommend"],
    &["diagnostic", "on_const"],
    &["diagnostic", "on_move"],
    &["diagnostic", "on_type_error"],
    &["diagnostic", "on_unimplemented"],
    &["diagnostic", "on_unknown"],
    &["diagnostic", "on_unmatched_args"],
    &["diagnostic", "opaque"],
    &["doc"],
    &["rustc_diagnostic_opaque"],
    &["rustc_on_unimplemented"],
];

/// The diagnostic attributes' groups, their bit in `State::diagnostic` and what they give.
const DIAGNOSTIC: [(&str, &str); 7] = [
    ("on_const", "OnConst"),
    ("on_move", "OnMove"),
    ("on_type_error", "OnTypeError"),
    ("on_unimplemented", "OnUnimplemented"),
    ("on_unknown", "OnUnknown"),
    ("on_unmatched_args", "OnUnmatchedArgs"),
    ("opaque", "Opaque"),
];

const ALIAS: &str = "`#[doc(alias = \"...\")]`";

/// The state of the parsers of `#[doc]` and of the diagnostic attributes, of one node.
#[derive(Default)]
pub(crate) struct State {
    doc: Doc,
    /// The meta items of the lists of `#[doc(..)]`: rustc's `nb_doc_attrs`.
    doc_items: usize,
    /// The diagnostic groups that parsed an attribute, by their index in `DIAGNOSTIC`.
    diagnostic: u8,
}

pub(crate) fn accept(state: &mut State, cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Accepted {
    let (group, feature) = match cx.builtin.path {
        ["doc"] => {
            if let ArgParser::List(list) = args {
                for item in list.mixed() {
                    if let Some(meta) = item.meta_item() {
                        state.doc_items += 1;
                        doc_item(&mut state.doc, cx, meta);
                    }
                }
            }
            return Accepted::Group("doc");
        }
        ["diagnostic", "do_not_recommend"] => return Accepted::Single(Some(Parsed::Other("DoNotRecommend"))),
        ["diagnostic", "on_const"] => ("on_const", Some("diagnostic_on_const")),
        ["diagnostic", "on_move"] => ("on_move", Some("diagnostic_on_move")),
        ["diagnostic", "on_type_error"] => ("on_type_error", Some("diagnostic_on_type_error")),
        ["diagnostic", "on_unimplemented"] | ["rustc_on_unimplemented"] => ("on_unimplemented", None),
        ["diagnostic", "on_unknown"] => ("on_unknown", Some("diagnostic_on_unknown")),
        ["diagnostic", "on_unmatched_args"] => ("on_unmatched_args", Some("diagnostic_on_unmatched_args")),
        ["diagnostic", "opaque"] => ("opaque", Some("diagnostic_opaque")),
        ["rustc_diagnostic_opaque"] => ("opaque", None),
        path => unreachable!("`{path:?}` is not the doc parsers'"),
    };
    if feature.is_none_or(|it| cx.feature(it))
        && let Some(bit) = DIAGNOSTIC.iter().position(|(it, _)| *it == group)
    {
        state.diagnostic |= 1 << bit;
    }
    Accepted::Group(group)
}

pub(crate) fn finalize(state: &mut State, group: &'static str, _fx: &mut Finalize<'_, '_, '_>) -> Option<Parsed> {
    if group == "doc" {
        return (state.doc_items != 0).then(|| Parsed::Doc(mem::take(&mut state.doc)));
    }
    let bit = DIAGNOSTIC.iter().position(|(it, _)| *it == group)?;
    (state.diagnostic & (1 << bit) != 0).then(|| Parsed::Other(DIAGNOSTIC[bit].1))
}

pub(crate) fn check(_group: GroupId, _first: usize, _fx: &mut Finalize<'_, '_, '_>, _parsed: &[ParsedAttr]) {}

/// rustc's `parse_single_doc_attr_item`: the errors of one element of a `#[doc(..)]`.
fn doc_item(doc: &mut Doc, cx: &mut Accept<'_, '_, '_>, meta: &MetaItem) {
    let path = meta.path.range;
    let args = &meta.args;
    match meta.path.word_sym() {
        Some("alias") => alias(doc, cx, path, args),
        Some(word @ ("inline" | "no_inline")) => {
            if matches!(args, ArgParser::NoArgs) {
                let inline = if word == "inline" { DocInline::Inline } else { DocInline::NoInline };
                doc.inline.push((inline, path));
            }
        }
        Some("masked") => {
            if matches!(args, ArgParser::NoArgs) {
                doc.masked = Some(path);
            }
        }
        Some("cfg") => cfg(cx, args),
        Some(word @ "keyword") => keyword_or_attribute(&mut doc.keyword, cx, path, args, word),
        Some(word @ "attribute") => keyword_or_attribute(&mut doc.attribute, cx, path, args, word),
        Some(word @ ("fake_variadic" | "search_unbox")) => {
            if !matches!(args, ArgParser::NoArgs) || !not_crate_level(cx, path, word) {
                return;
            }
            let slot = if word == "fake_variadic" { &mut doc.fake_variadic } else { &mut doc.search_unbox };
            *slot = Some(path);
        }
        Some("rust_logo") => {
            if matches!(args, ArgParser::NoArgs)
                && cx.target() == Target::Crate
                && cx.emits()
                && let Some(diag) = doc_rust_logo(cx.cx, &cx.session.features, cx.attr.in_file(path))
            {
                cx.emit_err(diag);
            }
        }
        Some("auto_cfg") => auto_cfg(cx, args),
        _ => {}
    }
}

/// rustc's `check_attr_not_crate_level`.
fn not_crate_level(cx: &mut Accept<'_, '_, '_>, range: TextRange, name: &str) -> bool {
    if cx.target() != Target::Crate {
        return true;
    }
    let diag = cx.err(None, format!("`#![doc({name} = \"...\")]` isn't allowed as a crate-level attribute"), range);
    cx.emit_err(diag);
    false
}

/// rustc's `parse_alias`.
fn alias(doc: &mut Doc, cx: &mut Accept<'_, '_, '_>, path: TextRange, args: &ArgParser) {
    match args {
        ArgParser::NoArgs => {
            let message = "doc alias attribute expects a string `#[doc(alias = \"a\")]` or a list of strings \
                           `#[doc(alias(\"a\", \"b\"))]`";
            let diag = cx.err(None, message, path);
            cx.emit_err(diag);
        }
        ArgParser::List(list) => {
            for item in list.mixed() {
                if let Some(alias) = cx.expect_string_literal_item(item) {
                    add_alias(doc, cx, alias, item.range());
                }
            }
        }
        ArgParser::NameValue(nv) => {
            if let Some(alias) = cx.expect_string_literal(nv) {
                add_alias(doc, cx, alias, nv.value_range());
            }
        }
    }
}

/// rustc's `add_alias`.
fn add_alias(doc: &mut Doc, cx: &mut Accept<'_, '_, '_>, alias: String, range: TextRange) {
    let message = if alias.is_empty() {
        format!("{ALIAS} attribute cannot have empty value")
    } else if let Some(c) = bad_alias_char(&alias) {
        format!("{c:?} character isn't allowed in {ALIAS}")
    } else if alias.starts_with(' ') || alias.ends_with(' ') {
        format!("{ALIAS} cannot start or end with ' '")
    } else {
        if !not_crate_level(cx, range, "alias") {
            return;
        }
        match doc.aliases.iter_mut().find(|(it, _)| *it == alias) {
            Some(entry) => entry.1 = range,
            None => doc.aliases.push((alias, range)),
        }
        return;
    };
    let diag = cx.err(None, message, range);
    cx.emit_err(diag);
}

/// The first character `#[doc(alias)]` refuses.
fn bad_alias_char(alias: &str) -> Option<char> {
    alias.chars().find(|&c| c == '"' || c == '\'' || (c.is_whitespace() && c != ' '))
}

/// rustc's `parse_keyword_and_attribute`; its lints are `lints::syntax::attrs::doc`'s.
fn keyword_or_attribute(
    slot: &mut Option<(String, TextRange)>,
    cx: &mut Accept<'_, '_, '_>,
    path: TextRange,
    args: &ArgParser,
    word: &str,
) {
    let Some(nv) = args.as_name_value() else { return };
    let Some(value) = nv.value_as_str() else { return };
    let (valid, message, help) = if word == "keyword" {
        (
            is_keyword(value),
            format!("nonexistent keyword `{value}` used in `#[doc(keyword = \"...\")]`"),
            "only existing keywords are allowed in core/std",
        )
    } else {
        (
            table::is_builtin_attr_name(value),
            format!("nonexistent builtin attribute `{value}` used in `#[doc(attribute = \"...\")]`"),
            "only existing builtin attributes are allowed in core/std",
        )
    };
    if !valid {
        let mut diag = cx.err(None, message, nv.value_range());
        diag.help(help);
        cx.emit_err(diag);
        return;
    }
    if slot.is_some() {
        cx.duplicate_key(path, word);
        return;
    }
    if !not_crate_level(cx, path, word) {
        return;
    }
    *slot = Some((value.to_owned(), path));
}

/// rustc's `Symbol::is_reserved` in the latest stable edition, `is_weak`, or `SelfTy`: what
/// `#[doc(keyword)]` may document.
fn is_keyword(name: &str) -> bool {
    matches!(
        name,
        "$crate"
            | "{{root}}"
            | "_"
            | "as"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
            | "async"
            | "await"
            | "dyn"
            | "gen"
            | "try"
            | "auto"
            | "builtin"
            | "catch"
            | "contract_ensures"
            | "contract_requires"
            | "default"
            | "macro_rules"
            | "raw"
            | "reuse"
            | "safe"
            | "union"
            | "yeet"
            | "SelfTy"
    )
}

/// rustc's `Symbol::is_path_segment_keyword`.
fn is_path_segment_keyword(name: &str) -> bool {
    matches!(name, "{{root}}" | "crate" | "self" | "Self" | "super" | "$crate")
}

/// rustc's `parse_cfg` of `#[doc(cfg(..))]`.
fn cfg(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) {
    let Some(list) = cx.expect_list(args, cx.attr_range()) else { return };
    let Some(single) = list.as_single() else {
        let target = cx.target().name();
        if list.is_empty() {
            cx.push_suggestion(format!("if the {target} should be disabled, use `#[cfg(false)]`"), list.range, "(false)");
        } else if let Some(args) = cx.cx.snippet(&cx.span(list.range)).map(str::to_owned) {
            cx.push_suggestion(
                format!("if the {target} should be enabled when all these predicates are, wrap them in `all`"),
                list.range,
                format!("(all{args})"),
            );
            cx.push_suggestion(
                format!(
                    "alternately, if the {target} should be enabled when any of these predicates are, wrap them in `any`"
                ),
                list.range,
                format!("(any{args})"),
            );
        }
        cx.expected_single_argument(list.range, list.len());
        return;
    };
    cfg_predicate(cx, single);
}

/// rustc's `parse_cfg_entry`: the errors of a predicate, with its gates where rustc gives them.
fn cfg_predicate(cx: &mut Accept<'_, '_, '_>, item: &MetaItemOrLit) {
    let meta = match item {
        MetaItemOrLit::Item(meta) => meta,
        MetaItemOrLit::Lit(lit) => {
            if !lit.is_bool() {
                cx.expected_identifier(lit.range);
            }
            return;
        }
    };
    let ArgParser::List(list) = &meta.args else {
        if meta.path.word_sym().is_none_or(is_path_segment_keyword) {
            cx.expected_identifier(meta.path.range);
            return;
        }
        gates(cx, item);
        if let Some(nv) = meta.args.as_name_value() {
            string_value(cx, nv);
        }
        return;
    };
    match meta.path.word_sym() {
        Some("not") => match list.as_single() {
            Some(single) => cfg_predicate(cx, single),
            None => cx.expected_single_argument(list.range, list.len()),
        },
        Some("any" | "all") => {
            for sub in list.mixed() {
                cfg_predicate(cx, sub);
            }
        }
        Some("target") => cfg_target(cx, meta, list),
        Some("version") => cfg_version(cx, item, list),
        _ => {
            let mut possibilities = vec!["any", "all", "not", "target"];
            if cx.feature("cfg_version") {
                possibilities.push("version");
            }
            cx.expected_specific_argument(meta.range(), &possibilities);
        }
    }
}

/// `parse_name_value`'s error: a value that is no string.
fn string_value(cx: &mut Accept<'_, '_, '_>, nv: &NameValue) {
    if nv.value_as_str().is_none() {
        cx.expected_string_literal(nv.value_range(), Some(nv.value_as_lit()));
    }
}

/// The gates of the predicate `item` alone (a leaf, an empty `target()` or a `version(..)`).
fn gates(cx: &mut Accept<'_, '_, '_>, item: &MetaItemOrLit) {
    if !cx.emits() {
        return;
    }
    for diag in cfg_entry(cx.cx, &cx.session.features, cx.attr.file, item) {
        cx.emit_err(diag);
    }
}

/// rustc's `parse_cfg_entry_target`.
fn cfg_target(cx: &mut Accept<'_, '_, '_>, meta: &MetaItem, list: &MetaList) {
    if cx.emits() {
        let compact = MetaItem {
            path: meta.path.clone(),
            args: ArgParser::List(MetaList { items: Vec::new(), range: list.range }),
        };
        gates(cx, &MetaItemOrLit::Item(compact));
    }
    for sub in list.mixed() {
        let Some((ident, nv)) = cx.expect_name_value_item(sub, None) else { continue };
        if is_path_segment_keyword(&ident.name) {
            cx.expected_identifier(ident.range);
            return;
        }
        if cx.emits() {
            let leaf = MetaItem {
                path: Path {
                    segments: vec![Ident { name: format!("target_{}", ident.name), range: ident.range }],
                    range: ident.range,
                },
                args: ArgParser::NameValue(nv.clone()),
            };
            gates(cx, &MetaItemOrLit::Item(leaf));
        }
        string_value(cx, nv);
    }
}

/// rustc's `parse_cfg_entry_version`.
fn cfg_version(cx: &mut Accept<'_, '_, '_>, item: &MetaItemOrLit, list: &MetaList) {
    gates(cx, item);
    let Some(version) = list.as_single() else {
        let diag = cx.err(None, "expected single version literal", list.range);
        cx.emit_err(diag);
        return;
    };
    let Some(lit) = version.as_lit() else {
        let diag = cx.err(None, "expected a version literal", version.range());
        cx.emit_err(diag);
        return;
    };
    let Some(text) = lit.str() else {
        let diag = cx.err(None, "expected a version literal", lit.range);
        cx.emit_err(diag);
        return;
    };
    if parse_version(text).is_none() {
        let mut diag =
            Diag::new(Level::Warning, None, "unknown version literal format, assuming it refers to a future version");
        diag.span = MultiSpan::of(cx.span(lit.range));
        cx.emit_err(diag);
    }
}

/// rustc's `parse_auto_cfg` and `parse_auto_cfg_values`: their errors, E0565; the rest are
/// lints.
fn auto_cfg(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) {
    let ArgParser::List(list) = args else { return };
    'main: for meta in list.mixed() {
        let Some(item) = meta.meta_item() else { continue };
        if !matches!(item.path.word_sym(), Some("hide" | "show")) {
            continue;
        }
        let ArgParser::List(list) = &item.args else { continue };
        let mut names = false;
        let mut values = false;
        for sub in list.mixed() {
            let Some(sub) = sub.meta_item() else { continue 'main };
            match &sub.args {
                ArgParser::NoArgs if !values => {
                    if sub.path.word_sym().is_none() {
                        cx.expected_identifier(sub.path.range);
                        continue 'main;
                    }
                    names = true;
                }
                ArgParser::List(values_list) if !values => {
                    if !sub.path.word_is("values") {
                        cx.expected_identifier(sub.path.range);
                        continue 'main;
                    }
                    if !names {
                        continue 'main;
                    }
                    for value in values_list.mixed() {
                        if let Some(value) = value.meta_item()
                            && value.ident().is_none()
                        {
                            cx.expected_identifier(value.path.range);
                        }
                    }
                    values = true;
                }
                _ => {}
            }
        }
    }
}

/// `check_doc_attrs`: the errors of the node's `#[doc(..)]`, `attr` being its first.
pub(crate) fn check_doc(fx: &mut Finalize<'_, '_, '_>, doc: &Doc, attr: usize) {
    for (alias, range) in &doc.aliases {
        check_alias(fx, attr, alias, *range);
    }
    if let Some((_, range)) = &doc.keyword {
        empty_module(fx, attr, *range, "keyword");
    }
    if let Some((_, range)) = &doc.attribute {
        empty_module(fx, attr, *range, "attribute");
    }
    if let Some(range) = doc.fake_variadic {
        fake_variadic(fx, attr, range);
    }
    if let Some(range) = doc.search_unbox {
        search_unbox(fx, attr, range);
    }
    inline(fx, attr, &doc.inline);
}

/// The span of `range`, in the node's `#[doc]` that has it; in `attr` when none does.
fn span_at(fx: &Finalize<'_, '_, '_>, attr: usize, range: TextRange) -> Span {
    let at = fx
        .owner
        .attrs
        .iter()
        .position(|it| it.meta().is_some() && it.is("doc") && it.range().contains_range(range))
        .unwrap_or(attr);
    fx.span(at, range)
}

fn error(fx: &mut Finalize<'_, '_, '_>, attr: usize, range: TextRange, message: impl Into<String>) {
    let span = span_at(fx, attr, range);
    fx.emit_err(Diag::error(None, message, span));
}

/// `check_doc_alias_value`.
fn check_alias(fx: &mut Finalize<'_, '_, '_>, attr: usize, alias: &str, range: TextRange) {
    let target = fx.target();
    let location = match target {
        Target::AssocTy if container_is(fx, SyntaxKind::IMPL) => Some("type alias in implementation block"),
        Target::AssocConst if container_is_trait_impl(fx) => Some("associated constant in trait implementation block"),
        Target::Param => return,
        Target::Expression
        | Target::Statement
        | Target::Arm
        | Target::ForeignMod
        | Target::Closure
        | Target::Impl { .. }
        | Target::WherePredicate => Some(target.name()),
        _ => None,
    };
    if let Some(location) = location {
        error(fx, attr, range, format!("{ALIAS} isn't allowed on {location}"));
        return;
    }
    if item_name(&fx.owner.node.value).is_some_and(|it| it == alias) {
        error(fx, attr, range, format!("`#[doc(alias = \"{alias}\"]` is the same as the item's name"));
    }
}

/// The trait or impl an associated item is in, through the macro call it is expanded from.
fn container(cx: &Cx<'_, '_>, node: InFile<&SyntaxNode>) -> Option<SyntaxNode> {
    let parent = node.value.parent();
    match parent {
        Some(parent) if parent.kind() == SyntaxKind::ASSOC_ITEM_LIST => parent.parent(),
        Some(parent) if parent.parent().is_some() => None,
        _ => {
            let call = node.file_id.macro_file()?.loc(cx.db).to_node(cx.db);
            container(cx, call.as_ref())
        }
    }
}

fn container_is(fx: &Finalize<'_, '_, '_>, kind: SyntaxKind) -> bool {
    container(fx.cx, fx.owner.node.as_ref()).is_some_and(|it| it.kind() == kind)
}

fn container_is_trait_impl(fx: &Finalize<'_, '_, '_>) -> bool {
    container(fx.cx, fx.owner.node.as_ref()).and_then(ast::Impl::cast).is_some_and(|it| it.trait_().is_some())
}

/// The name rustc's `tcx.opt_item_name` gives the node, as `check_doc_alias_value` compares it.
fn item_name(node: &SyntaxNode) -> Option<String> {
    let unraw = |text: &str| text.strip_prefix("r#").unwrap_or(text).to_owned();
    let rename = |rename: ast::Rename| match rename.name() {
        Some(name) => Some(unraw(name.text())),
        None => rename.underscore_token().map(|_| "_".to_owned()),
    };
    match node.kind() {
        SyntaxKind::EXTERN_CRATE => {
            let krate = ast::ExternCrate::cast(node.clone())?;
            match krate.rename() {
                Some(it) => rename(it),
                None => Some(unraw(krate.name_ref()?.text())),
            }
        }
        SyntaxKind::USE => {
            let tree = ast::Use::cast(node.clone())?.use_tree()?;
            if tree.use_tree_list().is_some() || tree.star_token().is_some() {
                return None;
            }
            match tree.rename() {
                Some(it) => rename(it),
                None => Some(unraw(tree.path()?.segment()?.name_ref()?.text())),
            }
        }
        SyntaxKind::TUPLE_FIELD => {
            let index = node.parent()?.children().filter(|it| it.kind() == SyntaxKind::TUPLE_FIELD).position(|it| it == *node)?;
            Some(index.to_string())
        }
        SyntaxKind::RECORD_PAT_FIELD => Some(unraw(&ast::RecordPatField::cast(node.clone())?.field_name()?.to_string())),
        SyntaxKind::RECORD_EXPR_FIELD => Some(unraw(ast::RecordExprField::cast(node.clone())?.field_name()?.text())),
        SyntaxKind::LIFETIME_PARAM => Some(ast::LifetimeParam::cast(node.clone())?.lifetime()?.text().to_owned()),
        _ => Some(unraw(ast::AnyHasName::cast(node.clone())?.name()?.text())),
    }
}

/// `check_doc_keyword_and_attribute`'s: `#[doc(keyword)]` and `#[doc(attribute)]` are of empty
/// modules.
fn empty_module(fx: &mut Finalize<'_, '_, '_>, attr: usize, range: TextRange, name: &str) {
    let node = &fx.owner.node;
    let empty = match node.value.kind() {
        SyntaxKind::MODULE => ast::Module::cast(node.value.clone()).map(|module| {
            match fx.cx.sema.to_def2(InFile::new(node.file_id, &module)) {
                Some(def) => module_is_empty(fx.cx, def),
                None => module.item_list().is_none_or(|it| it.items().next().is_none()),
            }
        }),
        SyntaxKind::SOURCE_FILE if fx.target() == Target::Mod => {
            fx.cx.sema.hir_file_to_module_def(node.file_id).map(|def| module_is_empty(fx.cx, def))
        }
        _ => None,
    };
    let message = match empty {
        Some(true) => return,
        Some(false) => "should be used on empty modules",
        None => "should be used on modules",
    };
    error(fx, attr, range, format!("`#[doc({name} = \"...\")]` {message}"));
}

/// rustc's `item_kind.is_empty()` of a module: no item, `macro_rules!` included.
fn module_is_empty(cx: &Cx<'_, '_>, module: hir::Module) -> bool {
    let db = cx.db;
    module.declarations(db).is_empty()
        && module.impl_defs(db).is_empty()
        && module.use_decls(db).is_empty()
        && module.extern_crate_decls(db).is_empty()
        && module.extern_blocks(db).is_empty()
        && !module.legacy_macros(db).into_iter().any(|it| it.module(db) == module)
}

/// `check_doc_fake_variadic`.
fn fake_variadic(fx: &mut Finalize<'_, '_, '_>, attr: usize, range: TextRange) {
    let message = match ast::Impl::cast(fx.owner.node.value.clone()) {
        None => "`#[doc(keyword = \"...\")]` should be used on impl blocks",
        Some(imp) => {
            let self_ty = imp.self_ty().is_some_and(variadic_self_ty);
            if self_ty || imp.trait_().and_then(|it| single_type_arg(&it)).is_some_and(is_one_tuple) {
                return;
            }
            "`#[doc(fake_variadic)]` must be used on the first of a set of tuple or fn pointer trait impls with varying arity"
        }
    };
    error(fx, attr, range, message);
}

/// The one type argument of a path type's last segment; `Fn(A, B)`'s inputs as a tuple.
enum OneArg {
    Ty(ast::Type),
    Inputs(usize),
}

fn single_type_arg(ty: &ast::Type) -> Option<OneArg> {
    let ast::Type::PathType(path) = ty else { return None };
    let segment = path.path()?.segment()?;
    if let Some(list) = segment.parenthesized_arg_list() {
        return Some(OneArg::Inputs(list.type_args().count()));
    }
    let mut args = segment.generic_arg_list()?.generic_args();
    let (Some(ast::GenericArg::TypeArg(arg)), None) = (args.next(), args.next()) else { return None };
    arg.ty().map(OneArg::Ty)
}

fn is_one_tuple(arg: OneArg) -> bool {
    match arg {
        OneArg::Inputs(inputs) => inputs == 1,
        OneArg::Ty(ty) => matches!(unparen(ty), ast::Type::TupleType(tuple) if tuple.fields().count() == 1),
    }
}

/// `ty`, through its parentheses, as HIR has it.
fn unparen(mut ty: ast::Type) -> ast::Type {
    while let ast::Type::ParenType(paren) = &ty
        && let Some(inner) = paren.ty()
    {
        ty = inner;
    }
    ty
}

/// rustc's `is_valid` of `check_doc_fake_variadic`: a 1-tuple, a 1-argument fn pointer, or a
/// path of one type argument that is one.
fn variadic_self_ty(ty: ast::Type) -> bool {
    match unparen(ty) {
        ast::Type::TupleType(tuple) => tuple.fields().count() == 1,
        ast::Type::FnPtrType(fn_ptr) => fn_ptr.param_list().is_some_and(|it| it.params().count() == 1),
        ty => match single_type_arg(&ty) {
            Some(OneArg::Ty(ty)) => variadic_self_ty(ty),
            Some(OneArg::Inputs(inputs)) => inputs == 1,
            None => false,
        },
    }
}

/// `check_doc_search_unbox`.
fn search_unbox(fx: &mut Finalize<'_, '_, '_>, attr: usize, range: TextRange) {
    let node = fx.owner.node.value.clone();
    let generic = |list: Option<ast::GenericParamList>| list.is_some_and(|it| it.generic_params().next().is_some());
    let valid = match fx.target() {
        Target::Struct => ast::Struct::cast(node).is_some_and(|it| generic(it.generic_param_list())),
        Target::Enum => ast::Enum::cast(node).is_some_and(|it| generic(it.generic_param_list())),
        Target::TyAlias => ast::TypeAlias::cast(node).is_some_and(|it| generic(it.generic_param_list())),
        Target::Trait => ast::Trait::cast(node).is_some_and(|it| {
            generic(it.generic_param_list())
                || it
                    .assoc_item_list()
                    .is_some_and(|list| list.assoc_items().any(|it| matches!(it, ast::AssocItem::TypeAlias(_))))
        }),
        _ => false,
    };
    if !valid {
        error(fx, attr, range, "`#[doc(search_unbox)]` should be used on generic structs and enums");
    }
}

/// `check_doc_inline`'s error: `inline` and `no_inline` on one node.
fn inline(fx: &mut Finalize<'_, '_, '_>, attr: usize, inline: &[(DocInline, TextRange)]) {
    let [(first, first_range), rest @ ..] = inline else { return };
    let Some((_, other_range)) = rest.iter().find(|(it, _)| it != first) else { return };
    let first = span_at(fx, attr, *first_range);
    let other = span_at(fx, attr, *other_range);
    let mut diag = Diag::error(None, "conflicting doc inlining attributes", first.clone());
    diag.primary(other.clone());
    diag.span_label(first, "this attribute...");
    diag.span_label(other, "...conflicts with this attribute");
    diag.help("remove one of the conflicting attributes");
    fx.emit_err(diag);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_sorted() {
        assert!(PATHS.windows(2).all(|it| it[0] < it[1]));
    }

    #[test]
    fn every_diagnostic_group_fits_the_bitset() {
        assert!(DIAGNOSTIC.len() <= u8::BITS as usize);
    }

    #[test]
    fn keywords_are_reserved_weak_or_self_ty() {
        for name in ["match", "SelfTy", "union", "gen", "yeet", "{{root}}", "_"] {
            assert!(is_keyword(name), "{name}");
        }
        for name in ["CookieMonster", "tadam", "selfty", "r#match", "Self_"] {
            assert!(!is_keyword(name), "{name}");
        }
    }

    #[test]
    fn an_alias_has_no_quote_and_no_whitespace_but_space() {
        assert_eq!(bad_alias_char("a b"), None);
        assert_eq!(bad_alias_char("a\tb"), Some('\t'));
        assert_eq!(bad_alias_char("\"x"), Some('"'));
        assert_eq!(bad_alias_char("it's"), Some('\''));
        assert_eq!(format!("{:?}", '\n'), "'\\n'");
    }
}
