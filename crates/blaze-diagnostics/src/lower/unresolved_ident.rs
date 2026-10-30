//! E0425: a name that resolves to no value where it is used (`x`, `f()`, an implicit capture
//! `{x}` of `format_args!`), with "not found in this scope", the name in scope of a similar
//! spelling, and the items of that name elsewhere to import. E0424 for `self` where there is no
//! `self` parameter, and E0531 for a name a pattern needs.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late/diagnostics.rs`
//! (`smart_resolve_report_errors`, `suggest_self_value`, `suggest_typo`,
//! `lookup_typo_candidate`), `compiler/rustc_resolve/src/late.rs` (`PathSource::descr_expected`,
//! `is_expected`, `error_code`), `compiler/rustc_resolve/src/diagnostics/impls.rs`
//! (`add_typo_suggestion`, `lookup_import_candidates`, `show_candidates`, `UsePlacementFinder`),
//! `compiler/rustc_hir/src/def.rs` (`Res::descr`) and `compiler/rustc_span/src/source_map.rs`
//! (`guess_head_span`) (MIT or Apache-2.0).

use either::Either;
use hir::{HasAttrs as _, InFile};
use span::Edition;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, TextRange, TextSize,
    ast::{self, HasModuleItem as _, HasName as _},
};

use crate::{
    cx::{Cx, item_range},
    lints::code_fact,
    lower::{Found, Phase, unresolved_field::find_best_match_for_name},
    model::{Applicability, Diag, MultiSpan, Span, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnresolvedIdent) -> Vec<Found> {
    let (ptr, in_node) = d.node.value;
    let ptr = InFile::new(d.node.file_id, ptr);
    let node = cx.node(ptr);
    let base = cx.span_of_ptr(ptr);
    let (span, name) = match in_node {
        Some(range) => {
            let text = node.value.as_ref().either(|it| it.syntax().to_string(), |it| it.syntax().to_string());
            let name = text.get(usize::from(range.start())..usize::from(range.end())).unwrap_or_default().to_owned();
            let span = match cx.snippet(&base) == Some(text.as_str()) {
                true => base.with_lo(base.lo + u32::from(range.start())).with_hi(base.lo + u32::from(range.end())),
                false => base.clone(),
            };
            (span, name)
        }
        None => (base.clone(), written_name(&node.value)),
    };
    let source = Source::of(&node.value, in_node.is_some(), &name);
    let syntax = node.value.as_ref().either(|it| it.syntax().clone(), |it| it.syntax().clone());
    let scope = cx.sema.scope(&syntax);
    let module = scope.as_ref().map(|it| it.module());

    let mut diag = if name == "self" {
        self_value(cx, &syntax, d.node.file_id, source, span.clone())
    } else {
        let message = format!("cannot find {} `{name}` in this scope", source.expected());
        let mut diag = Diag::error(Some(source.code()), message, span.clone());
        let suggested = scope.as_ref().is_some_and(|scope| typo(cx, &mut diag, scope, source, &name, &span));
        if !suggested {
            diag.span_label(span.clone(), "not found in this scope");
        }
        if let Some(module) = module {
            import_candidates(cx, &mut diag, module, source, &name);
        }
        diag
    };
    if let Some(module) = module {
        diag.fact(cx.resolves_fact(module, &name), format!("what `{name}` names in its scope"));
    }
    code_fact(cx, &mut diag, InFile::new(d.node.file_id, d.node.value.0.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Resolve)]
}

/// The name a path expression or pattern is written with.
fn written_name(node: &Either<ast::Expr, ast::Pat>) -> String {
    let path = match node {
        Either::Left(ast::Expr::PathExpr(it)) => it.path(),
        Either::Right(ast::Pat::PathPat(it)) => it.path(),
        Either::Right(ast::Pat::TupleStructPat(it)) => it.path(),
        Either::Right(ast::Pat::IdentPat(it)) => return it.name().map_or_else(String::new, |it| it.text().to_owned()),
        Either::Left(it) => return it.syntax().to_string(),
        Either::Right(it) => return it.syntax().to_string(),
    };
    match path.as_ref().and_then(|it| it.segment()) {
        Some(segment) if segment.self_token().is_some() => "self".to_owned(),
        Some(segment) => segment.name_ref().map_or_else(String::new, |it| it.text().to_owned()),
        None => String::new(),
    }
}

/// What rustc's resolver looked for the name as: its `PathSource`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Value,
    /// The callee of a call; with whether the name starts upper case.
    Callee(bool),
    Pat,
    TupleStruct,
}

impl Source {
    fn of(node: &Either<ast::Expr, ast::Pat>, captured: bool, name: &str) -> Source {
        match node {
            Either::Left(_) if captured => Source::Value,
            Either::Left(expr) => {
                let callee = expr
                    .syntax()
                    .parent()
                    .and_then(ast::CallExpr::cast)
                    .and_then(|call| call.expr())
                    .is_some_and(|callee| callee.syntax() == expr.syntax());
                match callee {
                    true => Source::Callee(name.chars().next().is_some_and(char::is_uppercase)),
                    false => Source::Value,
                }
            }
            Either::Right(ast::Pat::TupleStructPat(_)) => Source::TupleStruct,
            Either::Right(_) => Source::Pat,
        }
    }

    /// `descr_expected`.
    fn expected(self) -> &'static str {
        match self {
            Source::Value => "value",
            Source::Callee(false) => "function",
            Source::Callee(true) => "function, tuple struct or tuple variant",
            Source::Pat => "unit struct, unit variant or constant",
            Source::TupleStruct => "tuple struct or tuple variant",
        }
    }

    /// `error_code` with no resolution in the namespace.
    fn code(self) -> &'static str {
        match self {
            Source::Value | Source::Callee(_) => "E0425",
            Source::Pat | Source::TupleStruct => "E0531",
        }
    }

    /// `is_expected` of a thing rustc's `Res::descr` names `descr`.
    fn is_expected(self, descr: &str) -> bool {
        match self {
            Source::Value | Source::Callee(_) => true,
            Source::Pat => matches!(descr, "unit struct" | "unit variant" | "constant"),
            Source::TupleStruct => matches!(descr, "tuple struct" | "tuple variant"),
        }
    }
}

/// E0424 (`suggest_self_value`): `self` where no `self` parameter is.
fn self_value(cx: &Cx<'_, '_>, syntax: &SyntaxNode, file: hir::HirFileId, source: Source, span: Span) -> Diag {
    let message = format!("cannot find {} `self` in this scope", source.expected());
    let mut diag = Diag::error(Some("E0424"), message, span.clone());
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    if matches!(source, Source::Pat | Source::TupleStruct) {
        diag.span_label(span, "`self` value is a keyword and may not be bound to variables or shadowed");
        diag.note("a module named `self` exists in another namespace");
        return diag;
    }
    diag.span_label(span, "`self` value is a keyword only available in methods with a `self` parameter");
    if let Some(f) = syntax.ancestors().find_map(ast::Fn::cast) {
        let params = f.param_list();
        if params.as_ref().is_some_and(|it| it.self_param().is_some()) {
            diag.span_label(
                at(item_range(f.syntax())),
                "this function has a `self` parameter, but a macro invocation can only access identifiers it receives from parameters",
            );
        } else {
            let is_assoc = f.syntax().parent().is_some_and(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST);
            let doesnt = match is_assoc {
                true => {
                    let first = params.as_ref().and_then(|it| it.params().next());
                    let sugg = match (&first, params.as_ref().and_then(|it| it.l_paren_token())) {
                        (Some(first), _) => Some((at(first.syntax().text_range()).shrink_to_lo(), "&self, ")),
                        (None, Some(paren)) => Some((at(paren.text_range()).shrink_to_hi(), "&self")),
                        (None, None) => None,
                    };
                    if let Some((span, code)) = sugg {
                        diag.suggestion_verbose(
                            span,
                            "add a `self` receiver parameter to make the associated `fn` a method",
                            code,
                            Applicability::MaybeIncorrect,
                        );
                    }
                    "doesn't"
                }
                false => "can't",
            };
            if let Some(name) = f.name() {
                diag.span_label(at(name.syntax().text_range()), format!("this function {doesnt} have a `self` parameter"));
            }
        }
    }
    diag.note("a module named `self` exists in another namespace");
    diag
}

/// A name in scope a typo may have meant: rustc's `TypoSuggestion`.
struct Candidate {
    name: String,
    descr: &'static str,
    /// Where it is defined: an item's head, or a local's binding.
    def: Option<Span>,
    is_local: bool,
}

/// rustc's `Res::descr` of what a name in scope is, in the value namespace.
fn scope_descr(cx: &Cx<'_, '_>, def: &hir::ScopeDef<'_>) -> Option<&'static str> {
    Some(match def {
        hir::ScopeDef::Local(_) => "local variable",
        hir::ScopeDef::GenericParam(hir::GenericParam::ConstParam(_)) => "const parameter",
        hir::ScopeDef::ModuleDef(def) => return def_descr(cx, *def),
        _ => return None,
    })
}

/// rustc's `Res::descr` of an item in the value namespace: its constructor for a struct or
/// variant that has one.
fn def_descr(cx: &Cx<'_, '_>, def: hir::ModuleDef) -> Option<&'static str> {
    Some(match def {
        hir::ModuleDef::Function(_) => "function",
        hir::ModuleDef::Const(_) => "constant",
        hir::ModuleDef::Static(_) => "static",
        hir::ModuleDef::Adt(hir::Adt::Struct(it)) => match it.kind(cx.db) {
            hir::StructKind::Tuple => "tuple struct",
            hir::StructKind::Unit => "unit struct",
            hir::StructKind::Record => return None,
        },
        hir::ModuleDef::EnumVariant(it) => match it.kind(cx.db) {
            hir::StructKind::Tuple => "tuple variant",
            hir::StructKind::Unit => "unit variant",
            hir::StructKind::Record => return None,
        },
        _ => return None,
    })
}

/// rustc's `guess_head_span` of an item's `source_span`: up to its first `{`.
fn head_range(node: &SyntaxNode) -> TextRange {
    let range = item_range(node);
    let text = node.to_string();
    let base = node.text_range().start();
    let from = usize::from(range.start() - base);
    let item = &text[from..usize::from(range.end() - base)];
    let head = item.find('{').map_or(item, |at| &item[..at]).trim_end();
    TextRange::at(range.start(), TextSize::of(head))
}

/// The head span of an item's definition.
fn def_span(cx: &Cx<'_, '_>, def: hir::ModuleDef) -> Option<Span> {
    let source: InFile<SyntaxNode> = match def {
        hir::ModuleDef::Function(it) => cx.sema.source(it)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::Const(it) => cx.sema.source(it)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::Static(it) => cx.sema.source(it)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::Adt(it) => cx.sema.source(it)?.map(|it| it.syntax().clone()),
        hir::ModuleDef::EnumVariant(it) => cx.sema.source(it)?.map(|it| it.syntax().clone()),
        _ => return None,
    };
    Some(cx.span(source.map(|it| head_range(&it))))
}

/// rustc's `suggest_typo`: the name in scope, of a kind the place expects, closest to `name`;
/// whether it suggested one.
fn typo(cx: &Cx<'_, '_>, diag: &mut Diag, scope: &hir::SemanticsScope<'_>, source: Source, name: &str, span: &Span) -> bool {
    let mut candidates: Vec<Candidate> = Vec::new();
    scope.process_all_names(&mut |found, def| {
        let Some(descr) = scope_descr(cx, &def).filter(|it| source.is_expected(it)) else { return };
        let (def_at, is_local) = match &def {
            hir::ScopeDef::Local(local) => {
                let binding = local.primary_source(cx.db).name().map(|it| cx.span(it.map(|it| it.syntax().text_range())));
                (binding, true)
            }
            hir::ScopeDef::ModuleDef(it) => (def_span(cx, *it), false),
            _ => (None, false),
        };
        candidates.push(Candidate { name: found.as_str().to_owned(), descr, def: def_at, is_local });
    });
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<String> = candidates.iter().map(|it| it.name.clone()).collect();
    let Some(found) = find_best_match_for_name(&names, name, None) else { return false };
    if found == name || found == "_" {
        return false;
    }
    let Some(candidate) = candidates.into_iter().find(|it| it.name == found) else { return false };
    if let Some(def) = &candidate.def
        && def.file == span.file
        && def.lo < span.hi
        && span.lo < def.hi
    {
        return false;
    }
    let mut labelled = false;
    if !candidate.is_local
        && let Some(def) = &candidate.def
    {
        diag.span_label(def.clone(), format!("similarly named {} `{}` defined here", candidate.descr, candidate.name));
        labelled = true;
    }
    let snippet = cx.snippet(span);
    match (&candidate.def, candidate.name.strip_prefix('_')) {
        (Some(binding), Some(unused)) if snippet == Some(unused) && candidate.is_local => {
            if !labelled {
                diag.span_label(binding.clone(), format!("`{}` defined here", candidate.name));
            }
            diag.suggestion_verbose(
                binding.clone(),
                format!("the leading underscore in `{}` marks it as unused, consider renaming it to `{unused}`", candidate.name),
                unused,
                Applicability::MaybeIncorrect,
            );
        }
        _ => {
            diag.suggestion_verbose(
                span.clone(),
                format!("a {} with a similar name exists", candidate.descr),
                candidate.name,
                Applicability::MaybeIncorrect,
            );
        }
    }
    true
}

/// An item named as the name looked for, of the crate or one it depends on: rustc's
/// `ImportSuggestion`.
struct Import {
    path: String,
    /// Of this crate: rustc lists those first, by their `crate::` path.
    of_crate: bool,
    descr: &'static str,
    def: hir::ModuleDef,
}

/// rustc's `lookup_import_candidates` and `show_candidates`: the items of the name in this
/// crate and those it names as dependencies, to `use`, or that exist but are not visible here.
fn import_candidates(cx: &Cx<'_, '_>, diag: &mut Diag, module: hir::Module, source: Source, name: &str) {
    let cfg = hir::FindPathConfig {
        prefer_no_std: false,
        prefer_prelude: false,
        prefer_absolute: false,
        allow_unstable: true,
    };
    let crates = std::iter::once(cx.krate).chain(cx.krate.dependencies(cx.db).into_iter().map(|it| it.krate));
    let mut accessible: Vec<Import> = Vec::new();
    let mut inaccessible: Vec<Import> = Vec::new();
    for krate in crates {
        let local = krate == cx.krate;
        for of in krate.modules(cx.db) {
            for def in of.declarations(cx.db) {
                let mut consider = |def: hir::ModuleDef| {
                    if def.name(cx.db).is_none_or(|it| it.as_str() != name) {
                        return;
                    }
                    let Some(descr) = def_descr(cx, def).filter(|it| source.is_expected(it)) else { return };
                    let hidden = is_doc_hidden(cx, def);
                    let item = hir::ItemInNs::Values(def);
                    match module.find_use_path(cx.db, item, hir::PrefixKind::ByCrate, cfg) {
                        Some(path) if local || !hidden => {
                            let path = path.display(cx.db, cx.edition).to_string();
                            let of_crate = path.starts_with("crate::");
                            accessible.push(Import { path: in_edition(cx, path), of_crate, descr, def });
                        }
                        Some(_) => {}
                        None if local => {
                            let path = def.canonical_path(cx.db, cx.edition).unwrap_or_else(|| name.to_owned());
                            let path = in_edition(cx, format!("crate::{path}"));
                            inaccessible.push(Import { path, of_crate: true, descr, def });
                        }
                        None => {}
                    }
                };
                match def {
                    hir::ModuleDef::Adt(hir::Adt::Enum(it)) => {
                        it.variants(cx.db).into_iter().for_each(|it| consider(hir::ModuleDef::EnumVariant(it)));
                    }
                    def => consider(def),
                }
            }
        }
    }
    for found in [&mut accessible, &mut inaccessible] {
        found.sort_by(|a, b| a.path.cmp(&b.path));
        found.dedup_by(|a, b| a.path == b.path);
        let (mut local, rest): (Vec<Import>, Vec<Import>) = found.drain(..).partition(|it| it.of_crate);
        let (core, rest): (Vec<Import>, Vec<Import>) = rest.into_iter().partition(|it| it.path.starts_with("core::"));
        let (std, foreign): (Vec<Import>, Vec<Import>) = rest.into_iter().partition(|it| it.path.starts_with("std::"));
        let both = std.len() == core.len();
        local.extend(std);
        if !both {
            local.extend(core);
        }
        local.extend(foreign);
        *found = local;
    }
    if !accessible.is_empty() {
        let msg = match accessible.as_slice() {
            [only] => format!("consider importing this {}", only.descr),
            many => {
                let kind = match many.iter().all(|it| it.descr == many[0].descr) {
                    true => many[0].descr,
                    false => "item",
                };
                let s = if kind.ends_with('s') { "es" } else { "s" };
                format!("consider importing one of these {kind}{s}")
            }
        };
        match use_placement(cx, module) {
            Some((span, found_use)) => {
                let newline = if found_use { "" } else { "\n" };
                diag.suggestions(
                    span,
                    msg,
                    accessible.iter().map(|it| format!("use {};\n{newline}", it.path)),
                    Applicability::MaybeIncorrect,
                    SuggestionStyle::ShowAlways,
                );
            }
            None => {
                let paths: String = accessible.iter().map(|it| format!("\n{}", it.path)).collect();
                diag.help(format!("{msg}:{paths}"));
            }
        }
    }
    match inaccessible.as_slice() {
        [] => {}
        [only] => {
            let msg = format!("{} `{}` exists but is inaccessible", only.descr, only.path);
            match def_span(cx, only.def) {
                Some(span) => {
                    let mut multi = MultiSpan::of(span.clone());
                    multi.labels.push((span, "not accessible".to_owned()));
                    diag.span_note(multi, msg);
                }
                None => {
                    diag.note(msg);
                }
            }
        }
        many => {
            let descr = match many.iter().all(|it| it.descr == many[0].descr) {
                true => many[0].descr,
                false => "item",
            };
            let plural = if descr.ends_with('s') { format!("{descr}es") } else { format!("{descr}s") };
            let mut msg = format!("these {plural} exist but are inaccessible");
            let mut multi = MultiSpan::default();
            let mut colon = false;
            for it in many {
                match def_span(cx, it.def) {
                    Some(span) => {
                        multi.primary.push(span.clone());
                        multi.labels.push((span, format!("`{}`: not accessible", it.path)));
                    }
                    None => {
                        if !colon {
                            msg.push(':');
                            colon = true;
                        }
                        msg.push('\n');
                        msg.push_str(&it.path);
                    }
                }
            }
            diag.span_note(multi, msg);
        }
    }
}

/// Whether an item is `#[doc(hidden)]`, which rustc does not suggest importing from another
/// crate.
fn is_doc_hidden(cx: &Cx<'_, '_>, def: hir::ModuleDef) -> bool {
    match def {
        hir::ModuleDef::Function(it) => it.attrs(cx.db).is_doc_hidden(),
        hir::ModuleDef::Const(it) => it.attrs(cx.db).is_doc_hidden(),
        hir::ModuleDef::Static(it) => it.attrs(cx.db).is_doc_hidden(),
        hir::ModuleDef::Adt(it) => it.attrs(cx.db).is_doc_hidden(),
        hir::ModuleDef::EnumVariant(it) => it.attrs(cx.db).is_doc_hidden(),
        _ => false,
    }
}

/// A path to import as the crate's edition writes it: from the crate root without `crate::`
/// in 2015.
fn in_edition(cx: &Cx<'_, '_>, path: String) -> String {
    match (cx.edition, path.strip_prefix("crate::")) {
        (Edition::Edition2015, Some(rest)) => rest.to_owned(),
        _ => path,
    }
}

/// rustc's `UsePlacementFinder`: where a `use` goes in the module (not a block) `module` is
/// in, the first `use` item there or else its first item, and whether it was a `use`.
fn use_placement(cx: &Cx<'_, '_>, module: hir::Module) -> Option<(Span, bool)> {
    let module = module.nearest_non_block_module(cx.db);
    let source = module.definition_source(cx.db);
    let items: Vec<ast::Item> = match &source.value {
        hir::ModuleSource::SourceFile(it) => it.items().collect(),
        hir::ModuleSource::Module(it) => it.item_list()?.items().collect(),
        hir::ModuleSource::BlockExpr(_) => return None,
    };
    let (item, found_use) = match items.iter().find(|it| matches!(it, ast::Item::Use(_))) {
        Some(it) => (it, true),
        None => (items.first()?, false),
    };
    let start = item
        .syntax()
        .children_with_tokens()
        .find(|it| match it {
            syntax::NodeOrToken::Node(_) => true,
            syntax::NodeOrToken::Token(token) => {
                !token.kind().is_trivia() || ast::Comment::cast(token.clone()).is_some_and(|it| it.kind().doc.is_some())
            }
        })
        .map_or(item.syntax().text_range().start(), |it| it.text_range().start());
    Some((cx.span(InFile::new(source.file_id, TextRange::empty(start))), found_use))
}
