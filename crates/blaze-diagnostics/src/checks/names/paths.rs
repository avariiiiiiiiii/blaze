//! A path of a type or trait in a signature: each segment before the last must name a module
//! (or the type the rest is relative to), else E0433; the whole path must name what its place
//! expects, else E0425 "cannot find type" or E0405 "cannot find trait" when it names nothing,
//! E0573 "expected type, found .." or E0404 "expected trait, found .." when it names something
//! else; with the name in scope of a similar spelling and the items of that name to import.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`PathSource::descr_expected`,
//! `is_expected`, `error_code`), `late/diagnostics.rs` (`smart_resolve_report_errors`,
//! `make_base_error`, `suggest_typo`, `lookup_typo_candidate`), `diagnostics/impls.rs`
//! (`add_typo_suggestion`, `lookup_import_candidates`, `show_candidates`,
//! `UsePlacementFinder`) and `compiler/rustc_hir/src/def.rs` (`Res::descr`) (MIT or
//! Apache-2.0).

use hir::{HasAttrs as _, InFile, ModuleDef, PathResolution};
use span::Edition;
use syntax::{AstNode, SyntaxKind, SyntaxNode, TextRange, TextSize, ast};

use super::{
    Signature,
    failure::{self, At, ROOT, Segment},
    outer, privacy,
};
use crate::{
    cx::{Cx, item_range},
    lints::edit_distance::find_best_match_for_name,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// rustc's `PathSource` of a path in a signature.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    Type,
    Trait,
}

impl Source {
    fn of(ty: &ast::PathType) -> Source {
        let Some(parent) = ty.syntax().parent() else { return Source::Type };
        if ast::TypeBound::can_cast(parent.kind()) {
            return Source::Trait;
        }
        if let Some(imp) = ast::Impl::cast(parent.clone()) {
            let is_trait = imp.trait_().is_some_and(|it| it.syntax() == ty.syntax());
            return if is_trait { Source::Trait } else { Source::Type };
        }
        // `<T as Trait>`: the type after `as` is the trait.
        if ast::TypeAnchor::can_cast(parent.kind())
            && parent
                .children_with_tokens()
                .find(|it| it.kind() == SyntaxKind::AS_KW)
                .is_some_and(|it| it.text_range().start() < ty.syntax().text_range().start())
        {
            return Source::Trait;
        }
        Source::Type
    }

    fn expected(self) -> &'static str {
        match self {
            Source::Type => "type",
            Source::Trait => "trait",
        }
    }

    fn is_expected(self, res: &PathResolution<'_>) -> bool {
        match self {
            Source::Type => matches!(
                res,
                PathResolution::Def(
                    ModuleDef::Adt(_) | ModuleDef::Trait(_) | ModuleDef::TypeAlias(_) | ModuleDef::BuiltinType(_)
                ) | PathResolution::TypeParam(_)
                    | PathResolution::SelfType(_)
            ),
            Source::Trait => matches!(res, PathResolution::Def(ModuleDef::Trait(_))),
        }
    }

    /// rustc's `is_expected` of a name in scope rustc's `Res::descr` names `descr`.
    fn is_expected_descr(self, descr: &str) -> bool {
        match self {
            Source::Type => matches!(
                descr,
                "struct" | "enum" | "union" | "trait" | "type alias" | "builtin type" | "type parameter" | "self type"
            ),
            Source::Trait => descr == "trait",
        }
    }

    /// `error_code`, with or without a resolution of another kind.
    fn code(self, found: bool) -> &'static str {
        match (self, found) {
            (Source::Type, true) => "E0573",
            (Source::Type, false) => "E0425",
            (Source::Trait, true) => "E0404",
            (Source::Trait, false) => "E0405",
        }
    }
}

pub(super) fn check(cx: &Cx<'_, '_>, signature: &Signature, ty: &ast::PathType, path: &ast::Path) -> Option<Found> {
    let file = signature.node.file_id;
    let source = Source::of(ty);
    let mut prefixes: Vec<ast::Path> = std::iter::successors(Some(path.clone()), |it| it.qualifier()).collect();
    prefixes.reverse();
    let first = prefixes.first()?.segment()?;
    if first.type_anchor().is_some() {
        return None;
    }
    let root = first.coloncolon_token().map(|it| Segment { name: ROOT.to_owned(), range: it.text_range() });
    let offset = usize::from(root.is_some());
    let mut segments: Vec<Segment> = root.into_iter().collect();
    for prefix in &prefixes {
        segments.push(segment(&prefix.segment()?)?);
    }
    let place = At { file, node: path.syntax() };
    let current = cx.sema.scope(path.syntax())?.module();

    let mut module: Option<ModuleDef> = None;
    for (at, prefix) in prefixes[..prefixes.len() - 1].iter().enumerate() {
        match cx.sema.resolve_path(prefix) {
            Some(PathResolution::Def(def @ ModuleDef::Module(_))) => module = Some(def),
            Some(PathResolution::Def(def @ ModuleDef::Adt(hir::Adt::Enum(_)))) if at + 2 == prefixes.len() => {
                module = Some(def);
            }
            Some(res) => {
                let name = &segments[offset + at].name;
                if at == 0
                    && offset == 0
                    && is_from_outer(cx, signature, &res)
                    && let Some(diag) = outer::check(cx, signature, prefix, name, false)
                {
                    return Some(found(cx, diag, signature, current, name));
                }
                return None;
            }
            None => {
                let name = &segments[offset + at].name;
                if at == 0
                    && offset == 0
                    && let Some(diag) = outer::check(cx, signature, prefix, name, false)
                {
                    return Some(found(cx, diag, signature, current, name));
                }
                let diag = failure::diag(cx, &place, &segments, offset + at, module);
                return Some(found(cx, diag, signature, module_of(module).unwrap_or(current), name));
            }
        }
    }

    let last = segments.last()?.clone();
    let span = cx.span(InFile::new(file, path.syntax().text_range()));
    let per_ns = cx.sema.resolve_path_per_ns(path);
    let res = per_ns
        .as_ref()
        .and_then(|it| it.type_ns.clone().or_else(|| it.value_ns.clone()).or_else(|| it.macro_ns.clone()));
    if let Some(res) = res {
        if segments.len() == 1
            && is_from_outer(cx, signature, &res)
            && let Some(diag) = outer::check(cx, signature, path, &last.name, true)
        {
            return Some(found(cx, diag, signature, current, &last.name));
        }
        if source.is_expected(&res) {
            return privacy::check(cx, signature, &prefixes, &segments[offset..], current)
                .map(|diag| found(cx, diag, signature, current, &last.name));
        }
        let path_str = names_to_string(&segments);
        let message = format!("expected {}, found {} `{path_str}`", source.expected(), res_descr(cx, &res));
        let mut diag = Diag::error(Some(source.code(true)), message, span.clone());
        diag.span_label(span, format!("not a {}", source.expected()));
        if let PathResolution::TypeParam(param) = res
            && let Some(at) = param_span(cx, param)
        {
            diag.span_label(at, "found this type parameter");
        }
        return Some(found(cx, diag, signature, current, &last.name));
    }

    let single = segments.len() == 1;
    if single && let Some(diag) = outer::check(cx, signature, path, &last.name, true) {
        return Some(found(cx, diag, signature, current, &last.name));
    }
    let mut expected = source.expected();
    let mod_path = &segments[..segments.len() - 1];
    let (tick, mod_prefix, mod_str, in_module) = match mod_path {
        [] => ("", String::new(), "this scope".to_owned(), None),
        [first] if first.name == ROOT && cx.edition > Edition::Edition2015 => {
            expected = "crate";
            ("", String::new(), "the list of imported crates".to_owned(), None)
        }
        [first] if first.name == ROOT || first.name == "crate" => {
            ("", String::new(), "the crate root".to_owned(), Some(cx.krate.root_module(cx.db)))
        }
        _ => {
            let prefix = match module {
                Some(def) => format!("{} ", def_descr(cx, def).unwrap_or("module")),
                None => String::new(),
            };
            ("`", prefix, names_to_string(mod_path), module_of(module))
        }
    };
    let message = format!("cannot find {expected} `{}` in {mod_prefix}{tick}{mod_str}{tick}", last.name);
    let at = cx.span(InFile::new(file, last.range));
    let mut diag = Diag::error(Some(source.code(false)), message, at.clone());
    let lookup = in_module.unwrap_or(current);
    let suggested = typo(cx, &mut diag, path.syntax(), in_module, source, &last.name, &at);
    if !suggested {
        if ["true", "false"].contains(&last.name.to_lowercase().as_str()) {
            diag.suggestion_verbose(
                at.clone(),
                "you may want to use a bool value instead",
                last.name.to_lowercase(),
                Applicability::MaybeIncorrect,
            );
        }
        diag.span_label(at, format!("not found in {tick}{mod_str}{tick}"));
    }
    if single {
        import_candidates(cx, &mut diag, current, source, &last.name);
    }
    Some(found(cx, diag, signature, lookup, &last.name))
}

fn found(cx: &Cx<'_, '_>, mut diag: Diag, signature: &Signature, module: hir::Module, name: &str) -> Found {
    diag.fact(cx.resolves_fact(module, name), format!("what `{name}` names in its scope"));
    if let Some((fact, what)) = &signature.fact {
        diag.fact(fact.clone(), what.clone());
    }
    Found::new(diag, Phase::Resolve)
}

/// Whether a generic parameter or `Self` the path resolves to is declared outside the item
/// rustc resolves it in (the signature's item, or the impl or trait around an associated one).
fn is_from_outer(cx: &Cx<'_, '_>, signature: &Signature, res: &PathResolution<'_>) -> bool {
    let declared: Option<InFile<TextRange>> = match res {
        PathResolution::TypeParam(it) => cx.sema.source(it.merge()).map(|it| it.map(|it| either_range(&it))),
        PathResolution::ConstParam(it) => cx.sema.source(it.merge()).map(|it| it.map(|it| either_range(&it))),
        PathResolution::SelfType(it) => cx.sema.source(*it).map(|it| it.map(|it| it.syntax().text_range())),
        _ => return false,
    };
    let Some(declared) = declared else { return false };
    let inner = outer::inner_item(&signature.node.value);
    declared.file_id == signature.node.file_id && !inner.text_range().contains_range(declared.value)
}

fn either_range(node: &either::Either<ast::TypeOrConstParam, ast::Trait>) -> TextRange {
    match node {
        either::Either::Left(it) => it.syntax().text_range(),
        either::Either::Right(it) => it.syntax().text_range(),
    }
}

fn module_of(def: Option<ModuleDef>) -> Option<hir::Module> {
    match def {
        Some(ModuleDef::Module(it)) => Some(it),
        _ => None,
    }
}

/// A segment as rustc's resolver names it: its keyword or its name.
fn segment(segment: &ast::PathSegment) -> Option<Segment> {
    let (name, range) = match segment.kind()? {
        ast::PathSegmentKind::Name(it) => (it.text().to_owned(), it.syntax().text_range()),
        ast::PathSegmentKind::SelfTypeKw => ("Self".to_owned(), segment.self_type_token()?.text_range()),
        ast::PathSegmentKind::SelfKw => ("self".to_owned(), segment.self_token()?.text_range()),
        ast::PathSegmentKind::SuperKw => ("super".to_owned(), segment.super_token()?.text_range()),
        ast::PathSegmentKind::CrateKw => ("crate".to_owned(), segment.crate_token()?.text_range()),
        ast::PathSegmentKind::Type { .. } => return None,
    };
    Some(Segment { name, range })
}

/// rustc's `names_to_string`: the segments joined by `::`, the crate root as nothing.
fn names_to_string(segments: &[Segment]) -> String {
    let mut out = String::new();
    for (i, segment) in segments.iter().enumerate() {
        if i > 0 {
            out.push_str("::");
        }
        if segment.name != ROOT {
            out.push_str(&segment.name);
        }
    }
    out
}

/// rustc's `Res::descr` of a resolution.
fn res_descr(cx: &Cx<'_, '_>, res: &PathResolution<'_>) -> &'static str {
    match res {
        PathResolution::Def(def) => def_descr(cx, *def).unwrap_or("item"),
        PathResolution::Local(_) => "local variable",
        PathResolution::TypeParam(_) => "type parameter",
        PathResolution::ConstParam(_) => "const parameter",
        PathResolution::SelfType(_) => "self type",
        PathResolution::BuiltinAttr(_) => "built-in attribute",
        PathResolution::ToolModule(_) => "tool module",
        PathResolution::DeriveHelper(_) => "derive helper attribute",
    }
}

/// rustc's `Res::descr` of an item.
pub(super) fn def_descr(cx: &Cx<'_, '_>, def: ModuleDef) -> Option<&'static str> {
    Some(match def {
        ModuleDef::Module(_) => "module",
        ModuleDef::Function(_) => "function",
        ModuleDef::Adt(hir::Adt::Struct(_)) => "struct",
        ModuleDef::Adt(hir::Adt::Enum(_)) => "enum",
        ModuleDef::Adt(hir::Adt::Union(_)) => "union",
        ModuleDef::EnumVariant(_) => "variant",
        ModuleDef::Const(_) => "constant",
        ModuleDef::Static(_) => "static",
        ModuleDef::Trait(_) => "trait",
        ModuleDef::TypeAlias(_) => "type alias",
        ModuleDef::BuiltinType(_) => "builtin type",
        ModuleDef::Macro(it) => match it.kind(cx.db) {
            hir::MacroKind::Attr | hir::MacroKind::AttrBuiltIn => "attribute macro",
            hir::MacroKind::Derive | hir::MacroKind::DeriveBuiltIn => "derive macro",
            _ => "macro",
        },
    })
}

/// Where a type parameter is declared: its name.
fn param_span(cx: &Cx<'_, '_>, param: hir::TypeParam) -> Option<Span> {
    let source = cx.sema.source(param.merge())?;
    let range = match &source.value {
        either::Either::Left(ast::TypeOrConstParam::Type(it)) => it.name()?.syntax().text_range(),
        _ => return None,
    };
    Some(cx.span(source.with_value(range)))
}

/// The span rustc points at an item's definition with: the head of its item.
pub(super) fn def_span(cx: &Cx<'_, '_>, def: ModuleDef) -> Option<Span> {
    let source: InFile<SyntaxNode> = match def {
        ModuleDef::Module(it) => it.declaration_source(cx.db)?.map(|it| it.syntax().clone()),
        ModuleDef::Adt(it) => cx.sema.source(it)?.map(|it| it.syntax().clone()),
        ModuleDef::Trait(it) => cx.sema.source(it)?.map(|it| it.syntax().clone()),
        ModuleDef::TypeAlias(it) => cx.sema.source(it)?.map(|it| it.syntax().clone()),
        _ => return None,
    };
    Some(cx.span(source.map(|it| head_range(&it))))
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

/// A name a typo may have meant: rustc's `TypoSuggestion`.
struct Candidate {
    name: String,
    descr: &'static str,
    def: Option<Span>,
}

/// rustc's `Res::descr` of what a name in scope is, in the type namespace.
fn scope_descr(cx: &Cx<'_, '_>, def: &hir::ScopeDef<'_>) -> Option<(&'static str, Option<Span>)> {
    Some(match def {
        hir::ScopeDef::GenericParam(hir::GenericParam::TypeParam(it)) => ("type parameter", param_span(cx, *it)),
        hir::ScopeDef::ImplSelfType(_) | hir::ScopeDef::AdtSelfType(_) => ("self type", None),
        hir::ScopeDef::ModuleDef(def) => (def_descr(cx, *def)?, def_span(cx, *def)),
        _ => return None,
    })
}

/// rustc's `suggest_typo`: the name of the scope (or of the module the path names), of a kind
/// the place expects, closest to `name`; whether it suggested one.
fn typo(
    cx: &Cx<'_, '_>,
    diag: &mut Diag,
    node: &SyntaxNode,
    module: Option<hir::Module>,
    source: Source,
    name: &str,
    span: &Span,
) -> bool {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut consider = |found: &hir::Name, def: hir::ScopeDef<'_>| {
        let Some((descr, def_at)) = scope_descr(cx, &def).filter(|(it, _)| source.is_expected_descr(it)) else {
            return;
        };
        candidates.push(Candidate { name: found.as_str().to_owned(), descr, def: def_at });
    };
    match module {
        Some(module) => {
            for (found, def) in module.scope(cx.db, None) {
                consider(&found, def);
            }
        }
        None => {
            let Some(scope) = cx.sema.scope(node) else { return false };
            scope.process_all_names(&mut |found, def| consider(&found, def));
        }
    }
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = candidates.iter().map(|it| it.name.as_str()).collect();
    let Some(best) = find_best_match_for_name(&names, name, None) else { return false };
    if best == name || best == "_" {
        return false;
    }
    let best = best.to_owned();
    let Some(candidate) = candidates.into_iter().find(|it| it.name == best) else { return false };
    if let Some(def) = &candidate.def
        && def.file == span.file
        && def.lo < span.hi
        && span.lo < def.hi
    {
        return false;
    }
    if let Some(def) = &candidate.def {
        diag.span_label(def.clone(), format!("similarly named {} `{}` defined here", candidate.descr, candidate.name));
    }
    diag.suggestion_verbose(
        span.clone(),
        format!("a {} with a similar name exists", candidate.descr),
        candidate.name,
        Applicability::MaybeIncorrect,
    );
    true
}

/// An item named as the name looked for: rustc's `ImportSuggestion`.
struct Import {
    path: String,
    of_crate: bool,
    descr: &'static str,
    def: ModuleDef,
}

/// rustc's `lookup_import_candidates` and `show_candidates` in the type namespace.
fn import_candidates(cx: &Cx<'_, '_>, diag: &mut Diag, module: hir::Module, source: Source, name: &str) {
    let cfg = hir::FindPathConfig { prefer_no_std: false, prefer_prelude: false, prefer_absolute: false, allow_unstable: true };
    let crates = std::iter::once(cx.krate).chain(cx.krate.dependencies(cx.db).into_iter().map(|it| it.krate));
    let mut accessible: Vec<Import> = Vec::new();
    let mut inaccessible: Vec<Import> = Vec::new();
    for krate in crates {
        let local = krate == cx.krate;
        for of in krate.modules(cx.db) {
            for def in of.declarations(cx.db) {
                if def.name(cx.db).is_none_or(|it| it.as_str() != name) {
                    continue;
                }
                let Some(descr) = def_descr(cx, def).filter(|it| source.is_expected_descr(it)) else { continue };
                let hidden = is_doc_hidden(cx, def);
                match module.find_use_path(cx.db, hir::ItemInNs::Types(def), hir::PrefixKind::ByCrate, cfg) {
                    Some(path) if local || !hidden => {
                        let path = path.display(cx.db, cx.edition).to_string();
                        let of_crate = path.starts_with("crate::");
                        accessible.push(Import { path: in_edition(cx, path), of_crate, descr, def });
                    }
                    Some(_) => {}
                    None if local => {
                        let path = def.canonical_path(cx.db, cx.edition).unwrap_or_else(|| name.to_owned());
                        inaccessible.push(Import { path: in_edition(cx, format!("crate::{path}")), of_crate: true, descr, def });
                    }
                    None => {}
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
                let kind = if many.iter().all(|it| it.descr == many[0].descr) { many[0].descr } else { "item" };
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
                    let mut multi = crate::model::MultiSpan::of(span.clone());
                    multi.labels.push((span, "not accessible".to_owned()));
                    diag.span_note(multi, msg);
                }
                None => {
                    diag.note(msg);
                }
            }
        }
        many => {
            let descr = if many.iter().all(|it| it.descr == many[0].descr) { many[0].descr } else { "item" };
            let plural = if descr.ends_with('s') { format!("{descr}es") } else { format!("{descr}s") };
            let mut msg = format!("these {plural} exist but are inaccessible");
            let mut multi = crate::model::MultiSpan::default();
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
fn is_doc_hidden(cx: &Cx<'_, '_>, def: ModuleDef) -> bool {
    match def {
        ModuleDef::Adt(it) => it.attrs(cx.db).is_doc_hidden(),
        ModuleDef::Trait(it) => it.attrs(cx.db).is_doc_hidden(),
        ModuleDef::TypeAlias(it) => it.attrs(cx.db).is_doc_hidden(),
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
    use ast::HasModuleItem as _;
    use syntax::AstToken as _;
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
