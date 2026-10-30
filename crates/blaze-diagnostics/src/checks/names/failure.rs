//! E0433: a segment before the last of a path in a signature that names no module or type, as
//! rustc's late resolution reports it (`report_path_resolution_error` with the ribs of the
//! item, `opt_ns` the type namespace).
//!
//! Follows rustc's `compiler/rustc_resolve/src/diagnostics/impls.rs`
//! (`report_path_resolution_error`, `find_similarly_named_module_or_crate`) and
//! `compiler/rustc_span/src/source_map.rs` (`span_to_short_string`) (MIT or Apache-2.0).

use hir::{InFile, ItemInNs, ModPath, ModuleDef, Name, PathKind};
use span::Edition;
use syntax::{AstNode, AstToken as _, SyntaxKind, SyntaxNode, TextRange, ast};

pub(super) use crate::lower::unresolved_import::{ROOT, Segment};
use crate::{
    cx::Cx,
    lints::edit_distance::find_best_match_for_name,
    lower::unresolved_import::value_descr,
    model::{Applicability, Diag, Span},
};

/// Where a path is written: its file and a node in its scope.
pub(super) struct At<'a> {
    pub(super) file: hir::HirFileId,
    pub(super) node: &'a SyntaxNode,
}

/// The E0433 of segment `at` of `path`, `module` what the segments before it named.
pub(super) fn diag(cx: &Cx<'_, '_>, place: &At<'_>, path: &[Segment], at: usize, module: Option<ModuleDef>) -> Diag {
    let ident = &path[at];
    let name = ident.name.as_str();
    let span = |range: TextRange| cx.span(InFile::new(place.file, range));
    let scope = match path[..at].last() {
        Some(prev) if prev.name == ROOT => "the crate root".to_owned(),
        Some(prev) => format!("`{}`", prev.name),
        None => "this scope".to_owned(),
    };
    let mut message = format!("cannot find `{name}` in {scope}");
    let mut suggestion: Option<(Vec<(Span, String)>, String)> = None;
    let crate_root = cx.krate.root_module(cx.db);
    let label = if name == "super" {
        message = "too many leading `super` keywords".to_owned();
        "there are too many leading `super` keywords".to_owned()
    } else if module == Some(ModuleDef::Module(crate_root)) {
        if let Some(candidate) = module_candidate(cx, place, path, at) {
            suggestion = Some((vec![(span(ident.range), candidate)], "a similar path exists".to_owned()));
            "unresolved import".to_owned()
        } else if name == "core" {
            suggestion = Some((vec![(span(ident.range), "std".to_owned())], "try using `std` instead of `core`".to_owned()));
            "you might be missing crate `core`".to_owned()
        } else if name == "_" {
            message = "invalid crate or module name `_`".to_owned();
            "`_` is not a valid crate or module name".to_owned()
        } else if cx.edition == Edition::Edition2015 {
            message = format!("cannot find module or crate `{name}` in {scope}");
            suggestion = Some((
                vec![(crate_insert_span(cx), format!("extern crate {name};\n"))],
                format!("you might be missing a crate named `{name}`, add it to your project and import it in your code"),
            ));
            format!("use of unresolved module or unlinked crate `{name}`")
        } else {
            format!("could not find `{name}` in the crate root")
        }
    } else if at > 0 {
        let parent = match path[at - 1].name.as_str() {
            ROOT if cx.edition > Edition::Edition2015 => "the list of imported crates".to_owned(),
            ROOT | "crate" => "the crate root".to_owned(),
            parent => format!("`{parent}`"),
        };
        match value(cx, place, Some(&path[..at]), name) {
            Some(def) => format!("expected type, found {} `{name}` in {parent}", value_descr(cx, def)),
            None => format!("could not find `{name}` in {parent}"),
        }
    } else if name == "Self" {
        "`Self` is only available in impls, traits, and type definitions".to_owned()
    } else if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        message = format!("cannot find type `{name}` in {scope}");
        match value(cx, place, None, name).and_then(|def| Some((def, def_start(cx, def)?))) {
            Some((def, at)) => {
                let descr = value_descr(cx, def);
                let article = if descr.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
                format!("`{name}` is declared as {article} {descr} at `{at}`, not a type")
            }
            None => format!("use of undeclared type `{name}`"),
        }
    } else {
        if name == "alloc" {
            suggestion = Some((Vec::new(), "add `extern crate alloc` to use the `alloc` crate".to_owned()));
        }
        if suggestion.is_none() {
            suggestion = similar_module_or_crate(cx, place, name)
                .map(|it| (vec![(span(ident.range), it)], "there is a crate or module with a similar name".to_owned()));
        }
        message = format!("cannot find module or crate `{name}` in {scope}");
        match value(cx, place, None, name) {
            Some(def) => format!("{} `{name}` is not a crate or module", value_descr(cx, def)),
            None => {
                if suggestion.is_none() {
                    suggestion = Some((Vec::new(), format!("you might be missing a crate named `{name}`")));
                }
                format!("use of unresolved module or unlinked crate `{name}`")
            }
        }
    };
    let at_span = span(ident.range);
    let mut diag = Diag::error(Some("E0433"), message, at_span.clone());
    diag.span_label(at_span, label);
    match suggestion {
        Some((parts, message)) if parts.is_empty() => {
            diag.help(message);
        }
        Some((parts, message)) => {
            diag.multipart_suggestion(message, parts, Applicability::MaybeIncorrect);
        }
        None => {}
    }
    diag
}

/// The leading keywords of `path` as hir's path kind, and the names after them.
pub(super) fn kind_of(path: &[Segment]) -> (PathKind, &[Segment]) {
    let mut kind = PathKind::Plain;
    let mut rest = path;
    while let [first, tail @ ..] = rest {
        kind = match (first.name.as_str(), kind) {
            (ROOT, PathKind::Plain) if rest.len() == path.len() => PathKind::Abs,
            ("crate", PathKind::Plain) if rest.len() == path.len() => PathKind::Crate,
            ("self", PathKind::Plain) if rest.len() == path.len() => PathKind::Super(0),
            ("super", PathKind::Plain) if rest.len() == path.len() => PathKind::Super(1),
            ("super", PathKind::Super(n)) => PathKind::Super(n + 1),
            _ => break,
        };
        rest = tail;
    }
    (kind, rest)
}

/// What `name` names as a value in the module `scope` names, or where the path is.
fn value(cx: &Cx<'_, '_>, place: &At<'_>, scope: Option<&[Segment]>, name: &str) -> Option<ModuleDef> {
    let mut path: Vec<Segment> = scope.map_or_else(Vec::new, <[Segment]>::to_vec);
    path.push(Segment { name: name.to_owned(), range: TextRange::default() });
    let (kind, rest) = kind_of(&path);
    let path = ModPath::from_segments(kind, rest.iter().map(|it| Name::new_root(&it.name)));
    let mut items = cx.sema.resolve_mod_path(place.node, &path)?;
    items.find_map(|it| match it {
        ItemInNs::Values(def) => Some(def),
        _ => None,
    })
}

/// rustc's `span_to_short_string` of where a definition starts: `file:line:col`.
fn def_start(cx: &Cx<'_, '_>, def: ModuleDef) -> Option<String> {
    let source = match def {
        ModuleDef::Function(it) => cx.sema.source(it)?.map(|it| it.syntax().text_range()),
        ModuleDef::Const(it) => cx.sema.source(it)?.map(|it| it.syntax().text_range()),
        ModuleDef::Static(it) => cx.sema.source(it)?.map(|it| it.syntax().text_range()),
        _ => return None,
    };
    let span = cx.span(source);
    let file = cx.file_of(&span)?;
    let text = cx.file_text(file);
    let before = text.get(..span.lo as usize)?;
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |it| it.chars().count()) + 1;
    Some(format!("{}:{line}:{col}", span.file))
}

/// rustc's `lookup_import_candidates` of a module named as the segment `at` of `path`, as the
/// path that names it with what it shares with `path` left out.
fn module_candidate(cx: &Cx<'_, '_>, place: &At<'_>, path: &[Segment], at: usize) -> Option<String> {
    let db = cx.db;
    let name = path[at].name.as_str();
    let current = cx.sema.scope(place.node)?.module();
    let mut candidates: Vec<Vec<String>> = cx
        .krate
        .modules(db)
        .into_iter()
        .filter(|m| m.name(db).is_some_and(|it| it.as_str() == name))
        .filter(|m| hir::HasVisibility::is_visible_from(m, db, current))
        .map(|m| {
            let mut segments: Vec<String> = Vec::new();
            if cx.edition > Edition::Edition2015 {
                segments.push("crate".to_owned());
            }
            segments.extend(m.path_segments(db).map(|it| it.as_str().to_owned()));
            segments
        })
        .collect();
    candidates.sort_by_cached_key(|it| (it.len(), it.join("::")));
    let candidate = candidates.into_iter().next()?;
    let len = candidate.len();
    let start = (0..=at.min(len - 1)).find(|&i| path[i].name != candidate[i]).unwrap_or_default();
    Some(candidate[start..].join("::"))
}

/// rustc's `current_crate_outer_attr_insert_span`: before the first token after the crate
/// root's inner attributes.
fn crate_insert_span(cx: &Cx<'_, '_>) -> Span {
    let file = cx.krate.root_module(cx.db).definition_source_file_id(cx.db);
    let source = cx.sema.parse_or_expand(file);
    let is_token = |token: &syntax::SyntaxToken| match token.kind() {
        SyntaxKind::WHITESPACE | SyntaxKind::SHEBANG => false,
        SyntaxKind::COMMENT => ast::Comment::cast(token.clone())
            .and_then(|it| it.kind().doc)
            .is_some_and(|doc| doc == ast::AttrKind::Outer),
        _ => true,
    };
    let at = source
        .children_with_tokens()
        .find_map(|it| match it {
            syntax::NodeOrToken::Node(node) => {
                if ast::Attr::cast(node.clone()).is_some_and(|it| it.excl_token().is_some()) {
                    return None;
                }
                node.descendants_with_tokens().filter_map(syntax::NodeOrToken::into_token).find(is_token)
            }
            syntax::NodeOrToken::Token(token) => is_token(&token).then_some(token),
        })
        .map_or(source.text_range().end(), |it| it.text_range().start());
    cx.span(InFile::new(file, TextRange::empty(at)))
}

/// rustc's `find_similarly_named_module_or_crate`: a crate of the extern prelude or a module
/// below the one the path is in whose name is close to `name`.
fn similar_module_or_crate(cx: &Cx<'_, '_>, place: &At<'_>, name: &str) -> Option<String> {
    let db = cx.db;
    let current = cx.sema.scope(place.node)?.module().nearest_non_block_module(db);
    let mut candidates: Vec<Name> = cx.krate.dependencies(db).into_iter().map(|it| it.name).collect();
    for module in cx.krate.modules(db) {
        let below = std::iter::successors(module.parent(db), |it| it.parent(db)).any(|it| it == current);
        if below && let Some(it) = module.name(db) {
            candidates.push(it);
        }
    }
    candidates.retain(|it| !it.as_str().is_empty());
    candidates.sort_unstable_by(|a, b| a.symbol().cmp_order(b.symbol()));
    candidates.dedup();
    let candidates: Vec<&str> = candidates.iter().map(Name::as_str).collect();
    find_best_match_for_name(&candidates, name, None).filter(|it| *it != name).map(str::to_owned)
}
