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
use smallvec::SmallVec;
use span::Edition;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, TextRange,
    ast::{self, HasGenericArgs as _, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use super::{
    Signature,
    failure::{self, At, ROOT, Segment},
    outer, privacy,
};
use crate::{
    cx::Cx,
    lower::{
        Found, Phase,
        unresolved_ident::{Names, Ns, PathSource, def_descr, imports, show_imports, typo},
    },
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

    /// The resolver's `PathSource` the shared suggestions (`lower::unresolved_ident`) take.
    fn path(self) -> PathSource {
        match self {
            Source::Type => PathSource::Type,
            Source::Trait => PathSource::Trait,
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

/// The error of the path of type or trait `path` (of `ty`) in `signature`. What a path that
/// resolves costs is its resolutions alone: its segments' names are copied, and the module it
/// is in asked of Semantics, only for an error or a privacy test that needs them.
pub(super) fn check(cx: &Cx<'_, '_>, signature: &Signature, ty: &ast::PathType, path: &ast::Path) -> Option<Found> {
    let file = signature.node.file_id;
    let source = Source::of(ty);
    let mut prefixes: SmallVec<[ast::Path; 4]> = std::iter::successors(Some(path.clone()), |it| it.qualifier()).collect();
    prefixes.reverse();
    let first = prefixes.first()?.segment()?;
    if first.type_anchor().is_some() {
        return None;
    }
    if !prefixes.iter().all(|it| it.segment().is_some_and(|it| is_named(&it))) {
        return None;
    }
    let offset = usize::from(first.coloncolon_token().is_some());
    let single = offset == 0 && prefixes.len() == 1;
    if single && source == Source::Type && is_own_type_param(signature, path, &first) {
        return None;
    }
    let segments = || segments(&first, &prefixes);
    let current = || Some(cx.sema.scope(path.syntax())?.module());
    let place = At { file, node: path.syntax() };

    let mut module: Option<ModuleDef> = None;
    let mut defs: SmallVec<[ModuleDef; 4]> = SmallVec::new();
    for (at, prefix) in prefixes[..prefixes.len() - 1].iter().enumerate() {
        match cx.sema.resolve_path(prefix) {
            Some(PathResolution::Def(def @ ModuleDef::Module(_))) => {
                module = Some(def);
                defs.push(def);
            }
            Some(PathResolution::Def(def @ ModuleDef::Adt(hir::Adt::Enum(_)))) if at + 2 == prefixes.len() => {
                module = Some(def);
                defs.push(def);
            }
            Some(res) => {
                if at == 0 && offset == 0 && is_from_outer(cx, signature, &res) {
                    let current = current()?;
                    let segments = segments();
                    let name = &segments[at].name;
                    if let Some(diag) = outer::check(cx, signature, prefix, name, false) {
                        return Some(found(cx, diag, signature, current, name));
                    }
                }
                return None;
            }
            None => {
                let current = current()?;
                let segments = segments();
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

    let per_ns = cx.sema.resolve_path_per_ns(path);
    let res = per_ns
        .as_ref()
        .and_then(|it| it.type_ns.clone().or_else(|| it.value_ns.clone()).or_else(|| it.macro_ns.clone()));
    if let Some(res) = &res {
        if single && is_from_outer(cx, signature, res) {
            let current = current()?;
            let name = segments().pop()?.name;
            if let Some(diag) = outer::check(cx, signature, path, &name, true) {
                return Some(found(cx, diag, signature, current, &name));
            }
        }
        if source.is_expected(res) {
            // The first segment is looked up in scope, not in a module: no E0603 of one segment.
            if prefixes.len() == 1 {
                return None;
            }
            let (at, def, current) = privacy::private(cx, &prefixes, &defs, current)?;
            let segments = segments();
            let diag = privacy::diag(cx, signature, &segments[offset + at], def)?;
            return Some(found(cx, diag, signature, current, &segments.last()?.name));
        }
    }
    let current = current()?;
    let segments = segments();
    let last = segments.last()?.clone();
    if res.is_none()
        && single
        && let Some(diag) = outer::check(cx, signature, path, &last.name, true)
    {
        return Some(found(cx, diag, signature, current, &last.name));
    }
    let span = cx.span(InFile::new(file, path.syntax().text_range()));
    // What the path names in the type namespace makes the error "expected type, found ..";
    // in another one (`smart_resolve_report_errors`' `cross_namespace_res`) it is not found.
    let same = per_ns.as_ref().and_then(|it| it.type_ns.clone());
    let cross = if same.is_some() { None } else { res.clone() };
    let path_str = names_to_string(&segments);

    let (message, base, fallback, in_module) = match &same {
        Some(res) => (
            format!("expected {}, found {} `{path_str}`", source.expected(), res_descr(cx, res)),
            span.clone(),
            format!("not a {}", source.expected()),
            None,
        ),
        None => {
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
                        Some(def) => format!("{} ", def_descr(cx, Ns::Type, def).unwrap_or("module")),
                        None => String::new(),
                    };
                    ("`", prefix, names_to_string(mod_path), module_of(module))
                }
            };
            (
                format!("cannot find {expected} `{}` in {mod_prefix}{tick}{mod_str}{tick}", last.name),
                cx.span(InFile::new(file, last.range)),
                format!("not found in {tick}{mod_str}{tick}"),
                in_module,
            )
        }
    };
    let mut diag = Diag::error(Some(source.code(res.is_some())), message, base.clone());
    if let Some(res) = &cross {
        diag.note(format!("{} {} named `{path_str}` exists in another namespace", res_article(cx, res), res_descr(cx, res)));
    }
    if let Some(PathResolution::TypeParam(param)) = same
        && let Some(at) = param_span(cx, param)
    {
        diag.span_label(at, "found this type parameter");
    }

    // `try_lookup_name_relaxed`: the items of the name to import, else the enums of variants of
    // the name, and a name of a similar spelling.
    let own = match &res {
        Some(PathResolution::Def(def)) => Some(*def),
        _ => None,
    };
    let to_import = single.then(|| imports(cx, current, source.path(), &last.name, own)).filter(|it| !it.is_empty());
    let mut suggested: Vec<String> = Vec::new();
    if to_import.is_none() && source == Source::Type {
        suggested = variant_enums(cx, &mut diag, current, &last.name, res.is_none(), &span);
    }
    let at = cx.span(InFile::new(file, last.range));
    // `lookup_typo_candidate` of a longer path looks in the module its prefix names, if any.
    let lookup = if single { None } else { in_module.or(module_of(module)) };
    let typo = match lookup {
        Some(module) => typo(cx, &mut diag, Names::Module(module), source.path(), &last.name, &at, &suggested),
        None if single => cx.sema.scope(path.syntax()).is_some_and(|scope| {
            typo(cx, &mut diag, Names::Scope(&scope), source.path(), &last.name, &at, &suggested)
        }),
        None => false,
    };
    if !typo {
        if res.is_none() && ["true", "false"].contains(&last.name.to_lowercase().as_str()) {
            diag.suggestion_verbose(
                at,
                "you may want to use a bool value instead",
                last.name.to_lowercase(),
                Applicability::MaybeIncorrect,
            );
        }
        diag.span_label(base, fallback);
    }

    // `report_with_use_injections`: the imports, else a generic parameter to add.
    if let Some(to_import) = to_import {
        show_imports(cx, &mut diag, current, to_import, res.is_some());
    } else if res.is_none()
        && source == Source::Type
        && single
        && let Some((insert, sugg)) = missing_parameter(cx, signature, path, &last.name)
    {
        diag.suggestion_verbose(insert, "you might be missing a type parameter", sugg, Applicability::MaybeIncorrect);
    }
    Some(found(cx, diag, signature, lookup.unwrap_or(current), &last.name))
}

/// rustc's `Res::article`.
fn res_article(cx: &Cx<'_, '_>, res: &PathResolution<'_>) -> &'static str {
    use hir::AsAssocItem as _;
    match res {
        PathResolution::Def(ModuleDef::Adt(hir::Adt::Enum(_))) => "an",
        PathResolution::Def(ModuleDef::Function(it)) if it.as_assoc_item(cx.db).is_some() => "an",
        PathResolution::Def(ModuleDef::Const(it)) if it.as_assoc_item(cx.db).is_some() => "an",
        PathResolution::Def(ModuleDef::Macro(it))
            if matches!(it.kind(cx.db), hir::MacroKind::Attr | hir::MacroKind::AttrBuiltIn) =>
        {
            "an"
        }
        _ => "a",
    }
}

/// rustc's `suggest_adding_generic_parameter` for a path of a type: a parameter of the name
/// after the last one of the item (rustc's `current_item`), when the name is one capital and
/// the item a function (not `main`), struct, enum or union, or the path is in the generic
/// arguments of another and the item has generics; not where the generics hold the path.
fn missing_parameter(cx: &Cx<'_, '_>, signature: &Signature, path: &ast::Path, name: &str) -> Option<(Span, String)> {
    let segment = path.segment()?;
    if segment.generic_arg_list().is_some() || name == "Self" || name == "dyn" {
        return None;
    }
    let mut chars = name.chars();
    let single_upper = chars.next().is_some_and(char::is_uppercase) && chars.next().is_none();
    let in_args = path.syntax().ancestors().any(|it| ast::GenericArgList::can_cast(it.kind()));
    if !in_args && !single_upper {
        return None;
    }
    let item = ast::Item::cast(outer::inner_item(&signature.node.value))?;
    let suggests = match &item {
        ast::Item::Fn(it) if it.name().is_some_and(|it| it.text() == "main") => false,
        ast::Item::Fn(_) | ast::Item::Enum(_) | ast::Item::Struct(_) | ast::Item::Union(_) => true,
        ast::Item::Impl(_) => in_args || !single_upper,
        _ => !single_upper,
    };
    if !suggests {
        return None;
    }
    // `ItemKind::generics`: the items with generics; statics and the rest have none.
    let generic_item = match &item {
        ast::Item::Fn(_)
        | ast::Item::Enum(_)
        | ast::Item::Struct(_)
        | ast::Item::Union(_)
        | ast::Item::TypeAlias(_)
        | ast::Item::Trait(_)
        | ast::Item::TraitAlias(_)
        | ast::Item::Impl(_)
        | ast::Item::Const(_) => ast::AnyHasGenericParams::cast(item.syntax().clone())?,
        _ => return None,
    };
    let file = signature.node.file_id;
    let at = |range: TextRange| cx.span(InFile::new(file, TextRange::empty(range.end())));
    let Some(list) = generic_item.generic_param_list() else {
        let after = match ast::AnyHasName::cast(item.syntax().clone()).and_then(|it| it.name()) {
            Some(it) => it.syntax().text_range(),
            None => ast::Impl::cast(item.syntax().clone())?.impl_token()?.text_range(),
        };
        return Some((at(after), format!("<{name}>")));
    };
    if list.syntax().text_range().contains_range(path.syntax().text_range()) {
        return None;
    }
    let Some(param) = list.generic_params().last() else {
        return Some((at(list.syntax().text_range()), format!("<{name}>")));
    };
    let end = match &param {
        ast::GenericParam::TypeParam(it) => match it.type_bound_list().and_then(|it| it.bounds().last()) {
            Some(bound) => bound.syntax().text_range(),
            None => it.name()?.syntax().text_range(),
        },
        ast::GenericParam::LifetimeParam(it) => match it.type_bound_list().and_then(|it| it.bounds().last()) {
            Some(bound) => bound.syntax().text_range(),
            None => it.lifetime()?.syntax().text_range(),
        },
        ast::GenericParam::ConstParam(it) => match it.default_val() {
            Some(default) => default.syntax().text_range(),
            None => it.ty()?.syntax().text_range(),
        },
    };
    Some((at(end), format!(", {name}")))
}

fn found(cx: &Cx<'_, '_>, mut diag: Diag, signature: &Signature, module: hir::Module, name: &str) -> Found {
    diag.fact(cx.resolves_fact(module, name), format!("what `{name}` names in its scope"));
    signature.fact(cx, &mut diag);
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

/// Whether [`segment`] names `segment`, read with nothing copied.
fn is_named(segment: &ast::PathSegment) -> bool {
    match segment.kind() {
        Some(ast::PathSegmentKind::Name(_)) => true,
        Some(ast::PathSegmentKind::SelfTypeKw) => segment.self_type_token().is_some(),
        Some(ast::PathSegmentKind::SelfKw) => segment.self_token().is_some(),
        Some(ast::PathSegmentKind::SuperKw) => segment.super_token().is_some(),
        Some(ast::PathSegmentKind::CrateKw) => segment.crate_token().is_some(),
        Some(ast::PathSegmentKind::Type { .. }) | None => false,
    }
}

/// The segments of the path whose first segment is `first` and whose prefixes are `prefixes`
/// (outermost first), with the crate root of a leading `::`: for an error, once each prefix's
/// segment is known to be [`is_named`].
fn segments(first: &ast::PathSegment, prefixes: &[ast::Path]) -> Vec<Segment> {
    let root = first.coloncolon_token().map(|it| Segment { name: ROOT.to_owned(), range: it.text_range() });
    root.into_iter().chain(prefixes.iter().filter_map(|it| segment(&it.segment()?))).collect()
}

/// Whether the one-segment `path`, of first segment `first`, names a type parameter its item
/// (or the impl or trait around it) declares, outside the list that declares it: hir resolves
/// it to that parameter, which the place expects, and no error is given. Read from the syntax
/// before anything is asked of Semantics; only in a file of source, where no macro's hygiene
/// can make the name mean another.
fn is_own_type_param(signature: &Signature, path: &ast::Path, first: &ast::PathSegment) -> bool {
    if signature.node.file_id.is_macro() || first.generic_arg_list().is_some() {
        return false;
    }
    let Some(ast::PathSegmentKind::Name(name)) = first.kind() else { return false };
    let name = name.text();
    let at = path.syntax().text_range();
    let item = &signature.node.value;
    let inner = outer::inner_item(item);
    let declares = |node: &SyntaxNode| {
        let Some(list) = ast::AnyHasGenericParams::cast(node.clone()).and_then(|it| it.generic_param_list()) else {
            return false;
        };
        !list.syntax().text_range().contains_range(at)
            && list.type_or_const_params().any(|it| match it {
                ast::TypeOrConstParam::Type(param) => param.name().is_some_and(|it| it.text() == name),
                ast::TypeOrConstParam::Const(_) => false,
            })
    };
    declares(item) || (inner != *item && declares(&inner))
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
        PathResolution::Def(def) => def_descr(cx, Ns::Type, *def).unwrap_or("item"),
        PathResolution::Local(_) => "local variable",
        PathResolution::TypeParam(_) => "type parameter",
        PathResolution::ConstParam(_) => "const parameter",
        PathResolution::SelfType(_) => "self type",
        PathResolution::BuiltinAttr(_) => "built-in attribute",
        PathResolution::ToolModule(_) => "tool module",
        PathResolution::DeriveHelper(_) => "derive helper attribute",
    }
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

/// The path `module` imports `def` by (and whether it is from the crate root), or the path of
/// a local item it can't see; nothing for a hidden or unreachable foreign one.
fn import_path(cx: &Cx<'_, '_>, module: hir::Module, def: ModuleDef, local: bool) -> Option<Result<(String, bool), String>> {
    let cfg = hir::FindPathConfig { prefer_no_std: false, prefer_prelude: false, prefer_absolute: false, allow_unstable: true };
    match module.find_use_path(cx.db, hir::ItemInNs::Types(def), hir::PrefixKind::ByCrate, cfg) {
        Some(path) if local || !is_doc_hidden(cx, def) => {
            let path = path.display(cx.db, cx.edition).to_string();
            let of_crate = path.starts_with("crate::");
            Some(Ok((in_edition(cx, path), of_crate)))
        }
        Some(_) => None,
        None if local => {
            let path = def.canonical_path(cx.db, cx.edition)?;
            Some(Err(in_edition(cx, format!("crate::{path}"))))
        }
        None => None,
    }
}

/// `try_lookup_name_relaxed`'s enums of the variants named `name`, suggested for the path at
/// `span`; the enums' paths, which no typo then repeats. The variants are read from the
/// declarations of the name in the crate and its direct dependencies (`Paths::decls`, one
/// probe), not from every enum of every module.
fn variant_enums(cx: &Cx<'_, '_>, diag: &mut Diag, module: hir::Module, name: &str, unresolved: bool, span: &Span) -> Vec<String> {
    let mut found: Vec<(String, String)> = Vec::new();
    for decl in cx.paths().decls(cx).named(name) {
        let ModuleDef::EnumVariant(_) = decl.def else { continue };
        let path = match import_path(cx, module, decl.def, decl.local) {
            Some(Ok((path, _))) => path,
            _ => continue,
        };
        let Some((enum_path, _)) = path.rsplit_once("::") else { continue };
        if enum_path.starts_with("std::prelude::") {
            continue;
        }
        let enum_path = enum_path.to_owned();
        found.push((path, enum_path));
    }
    if found.is_empty() {
        return Vec::new();
    }
    found.sort();
    found.dedup();
    let preamble = if unresolved {
        let others = match found.len() {
            1 => String::new(),
            2 => " and 1 other".to_owned(),
            n => format!(" and {n} others"),
        };
        format!("there is an enum variant `{}`{others}; ", found[0].0)
    } else {
        String::new()
    };
    let enums: Vec<String> = found.into_iter().map(|(_, it)| it).collect();
    diag.suggestions(
        span.clone(),
        format!("{preamble}try using the variant's enum"),
        enums.clone(),
        Applicability::MachineApplicable,
        SuggestionStyle::ShowCode,
    );
    enums
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
