//! An import that does not resolve. hir says which use tree; why it fails is found again here
//! as rustc's resolver finds it, walking the import's path through hir's resolution of each
//! prefix: a segment before the last of the module path not found is E0433 on its own; the last
//! one not found, or the name not in the module found, is E0432 "unresolved import `a::b`",
//! one diagnostic for the failed imports of one `use` item, each with its label ("no `b` in
//! `a`", "use of unresolved module or unlinked crate `a`", "could not find `x` in `y`", ..)
//! and suggestion ("a similar path exists", "a similar name exists in the module", the
//! `extern crate` to add).
//!
//! Follows rustc's `compiler/rustc_resolve/src/imports.rs` (`finalize_imports`,
//! `finalize_import`, `import_path_to_string`), `diagnostics/impls.rs`
//! (`throw_unresolved_import_error`, `report_path_resolution_error`, `make_path_suggestion`,
//! `find_similarly_named_module_or_crate`, `into_struct_error` of `FailedToResolve`) and
//! `lib.rs` (`names_to_string`, `module_to_string`) (MIT or Apache-2.0).

use blaze_items::Fact;
use hir::{HasVisibility as _, InFile, ItemInNs, ModPath, ModuleDef, Name, PathKind, StructKind};
use span::Edition;
use syntax::{AstNode, SyntaxKind, SyntaxNode, TextRange, TextSize, ast};

use crate::{
    cx::Cx,
    lints::edit_distance::find_best_match_for_name,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

/// rustc's name of the crate root segment, `::` or implicit in a 2015 import.
pub(super) const ROOT: &str = "{{root}}";

/// rustc's upper limit on the labels of one diagnostic of unresolved imports.
const MAX_LABEL_COUNT: usize = 10;

/// A segment of a path as rustc's resolver has it, with where it is written.
#[derive(Clone, Debug)]
pub(super) struct Segment {
    pub(super) name: String,
    pub(super) range: TextRange,
}

/// An import as rustc's resolver has it: the module it is from and the name it takes.
#[derive(Clone, Debug)]
struct Import {
    /// rustc's `module_path`, a leading `{{root}}` included.
    module_path: Vec<Segment>,
    /// The name imported; none for a glob.
    source: Option<Segment>,
    /// rustc's `import.span`: the use tree, its rename included.
    range: TextRange,
}

/// What a module path resolved to: a module, an enum or a trait, or 2018's `::`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Def(ModuleDef),
    ExternPrelude,
}

/// A suggestion of rustc's: its parts, or none for one written as a help.
struct Suggestion {
    parts: Vec<(Span, String)>,
    message: String,
}

/// Why an import fails, as rustc reports it.
enum Failure {
    /// E0433 of a segment before the last of the module path.
    Path { at: TextRange, error: PathError },
    /// The import's part of an E0432.
    Import(ImportError),
}

/// rustc's `UnresolvedImportError`.
struct ImportError {
    span: TextRange,
    label_at: TextRange,
    path: String,
    label: Option<String>,
    note: Option<String>,
    suggestion: Option<Suggestion>,
    fact: Option<(Fact, String)>,
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnresolvedImport) -> Vec<Found> {
    let tree = cx.node(d.decl);
    let file = tree.file_id;
    let r = Resolver { cx, file, node: tree.value.syntax().clone() };
    let mine = r.failure(&r.import(&tree.value));
    let error = match mine {
        Some(Failure::Path { at, error }) => return vec![Found::new(path_diag(&r, at, error), Phase::Resolve)],
        Some(Failure::Import(error)) => error,
        None => {
            // hir finds no way to it where this walk finds one: still the error, as rustc's
            // message says it, with nothing it cannot say.
            let import = r.import(&tree.value);
            let path = import_path(&import, None);
            let diag = Diag::error(Some("E0432"), format!("unresolved import `{path}`"), r.span(import.range));
            return vec![Found::new(diag, Phase::Resolve)];
        }
    };
    // rustc gives the failed imports of one `use` item in one diagnostic, at the first: the
    // others give nothing of their own.
    let leaves = tree.value.syntax().ancestors().find_map(ast::Use::cast).map_or_else(Vec::new, |it| leaves(&it));
    let at = leaves.iter().position(|it| it.syntax() == tree.value.syntax()).unwrap_or(leaves.len());
    let import_error = |leaf: &ast::UseTree| {
        let r = Resolver { cx, file, node: leaf.syntax().clone() };
        match r.failure(&r.import(leaf)) {
            Some(Failure::Import(error)) => Some(error),
            _ => None,
        }
    };
    if leaves[..at].iter().any(|it| import_error(it).is_some()) {
        return Vec::new();
    }
    let mut group = vec![error];
    group.extend(leaves.iter().skip(at + 1).filter_map(import_error));
    vec![Found::new(unresolved_imports(&r, group), Phase::Resolve)]
}

/// rustc's `throw_unresolved_import_error` of the errors of one `use` item.
fn unresolved_imports(r: &Resolver<'_, '_, '_>, errors: Vec<ImportError>) -> Diag {
    let paths: Vec<String> = errors.iter().map(|it| format!("`{}`", it.path)).collect();
    let plural = if paths.len() == 1 { "" } else { "s" };
    let message = format!("unresolved import{plural} {}", paths.join(", "));
    let mut spans = errors.iter().map(|it| r.span(it.span));
    let mut diag = Diag::error(Some("E0432"), message, spans.next().expect("one error at least"));
    spans.for_each(|it| {
        diag.primary(it);
    });
    if let Some(note) = errors.last().and_then(|it| it.note.clone()) {
        diag.note(note);
    }
    for error in errors.into_iter().take(MAX_LABEL_COUNT) {
        if let Some(label) = error.label {
            diag.span_label(r.span(error.label_at), label);
        }
        if let Some((fact, what)) = error.fact {
            diag.fact(fact, what);
        }
        if let Some(suggestion) = error.suggestion {
            suggest(&mut diag, suggestion);
        }
    }
    diag
}

/// E0433 of a path's segment at `at`, as rustc's `FailedToResolve` gives it.
fn path_diag(r: &Resolver<'_, '_, '_>, at: TextRange, error: PathError) -> Diag {
    let at = r.span(at);
    let mut diag = Diag::error(Some("E0433"), error.message, at.clone());
    diag.span_label(at, error.label);
    if let Some(suggestion) = error.suggestion {
        suggest(&mut diag, suggestion);
    }
    if let Some((fact, what)) = error.fact {
        diag.fact(fact, what);
    }
    diag
}

/// rustc's error of a macro path of more than one segment that does not resolve
/// (`finalize_macro_resolutions`): E0433 at the segment not found, or at the whole path when it
/// goes into an enum or a trait. `expected` is the macro's kind as rustc names it, with its
/// article (`("a", "macro")`); `bang` whether it is called with `!`.
pub(super) fn macro_path_error(
    cx: &Cx<'_, '_>,
    file: hir::HirFileId,
    node: SyntaxNode,
    path: &[Segment],
    range: TextRange,
    expected: (&str, &str),
    bang: bool,
) -> Diag {
    let r = Resolver { cx, file, node };
    let (last, module_path) = path.split_last().expect("a path of one segment at least");
    let mut module = None;
    for (at, segment) in module_path.iter().enumerate() {
        match r.module(&module_path[..=at]) {
            Some(Scope::Def(def @ (ModuleDef::Adt(_) | ModuleDef::Trait(_)))) => {
                let (found_an, found_descr) = match def {
                    ModuleDef::Trait(_) => ("a", "trait"),
                    _ => ("an", "enum"),
                };
                let unresolved = path.len() - at - 1;
                let scope = match path[..unresolved].last() {
                    Some(prev) => format!("{found_descr} `{}`", prev.name),
                    None => found_descr.to_owned(),
                };
                let (expected_an, expected_descr) = expected;
                let message = format!("cannot find {expected_descr} `{}` in {scope}", path[unresolved].name);
                let span = r.span(range);
                let mut diag = Diag::error(Some("E0433"), message, span.clone());
                diag.span_label(span, format!("{expected_an} {expected_descr} can't exist within {found_an} {found_descr}"));
                return diag;
            }
            Some(found) => module = Some(found),
            None => return path_diag(&r, segment.range, r.path_error(path, at, module, true)),
        }
    }
    let mut diag = path_diag(&r, last.range, r.path_error(path, path.len() - 1, module, true));
    if bang && let Some(def) = r.value(Some(module_path), &last.name) {
        let bang_at = TextRange::at(last.range.end(), TextSize::of('!'));
        let names = names_to_string(path.iter().map(|it| it.name.as_str()));
        diag.multipart_suggestion(
            format!("{names} is not a macro, but a {}, try to remove `!`", value_descr(cx, def)),
            vec![(r.span(bang_at), String::new())],
            Applicability::MaybeIncorrect,
        );
    }
    diag
}

fn suggest(diag: &mut Diag, suggestion: Suggestion) {
    match suggestion.parts.is_empty() {
        true => diag.help(suggestion.message),
        false => diag.multipart_suggestion(suggestion.message, suggestion.parts, Applicability::MaybeIncorrect),
    };
}

/// The use trees of `item` that import one thing each, in order.
fn leaves(item: &ast::Use) -> Vec<ast::UseTree> {
    fn walk(tree: ast::UseTree, out: &mut Vec<ast::UseTree>) {
        match tree.use_tree_list() {
            Some(list) => list.use_trees().for_each(|it| walk(it, out)),
            None => out.push(tree),
        }
    }
    let mut out = Vec::new();
    if let Some(tree) = item.use_tree() {
        walk(tree, &mut out);
    }
    out
}

/// rustc's `import_path_to_string`: the module path up to the segment that failed, or with the
/// name imported.
fn import_path(import: &Import, failed: Option<usize>) -> String {
    let global = import.module_path.first().is_some_and(|it| it.name == ROOT);
    let names = &import.module_path[usize::from(global)..];
    let names: Vec<&str> = names.iter().map(|it| it.name.as_str()).collect();
    if let Some(at) = failed {
        let at = at - usize::from(global);
        return names[..=at].join("::");
    }
    let source = import.source.as_ref().map_or("*", |it| it.name.as_str());
    match names.is_empty() {
        true => source.to_owned(),
        false => format!("{}::{source}", names.join("::")),
    }
}

/// rustc's `names_to_string`: the names joined by `::`, `{{root}}` written as nothing.
fn names_to_string<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for (i, name) in names.into_iter().enumerate().filter(|(_, it)| *it != ROOT) {
        if i > 0 {
            out.push_str("::");
        }
        out.push_str(name);
    }
    out
}

fn is_path_segment_keyword(name: &str) -> bool {
    matches!(name, ROOT | "crate" | "self" | "super" | "Self" | "$crate")
}

/// Resolution of the paths of the import at `node` through hir's resolver.
struct Resolver<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    file: hir::HirFileId,
    node: SyntaxNode,
}

impl Resolver<'_, '_, '_> {
    fn span(&self, range: TextRange) -> Span {
        self.cx.span(InFile::new(self.file, range))
    }

    fn edition(&self) -> Edition {
        self.cx.edition
    }

    /// The module the import is in: `self` of its paths.
    fn current(&self) -> Option<hir::Module> {
        Some(self.cx.sema.scope(&self.node)?.module().nearest_non_block_module(self.cx.db))
    }

    /// rustc's import of the leaf use tree `tree`: the paths of the trees it is nested in and
    /// its own, `{{root}}` before a 2015 path that does not start with a keyword.
    fn import(&self, tree: &ast::UseTree) -> Import {
        let mut trees = vec![tree.clone()];
        let mut at = tree.clone();
        while let Some(parent) = at.parent_use_tree_list().map(|it| it.parent_use_tree()) {
            trees.push(parent.clone());
            at = parent;
        }
        trees.reverse();
        let mut segments: Vec<Segment> = Vec::new();
        for tree in &trees {
            match tree.path() {
                Some(path) => segments.extend(path_segments(&path)),
                None => {
                    if let Some(token) = tree.coloncolon_token()
                        && segments.is_empty()
                    {
                        segments.push(Segment { name: ROOT.to_owned(), range: token.text_range() });
                    }
                }
            }
        }
        if self.edition() == Edition::Edition2015
            && let Some(first) = segments.first()
            && !is_path_segment_keyword(&first.name)
        {
            let range = TextRange::empty(first.range.start());
            segments.insert(0, Segment { name: ROOT.to_owned(), range });
        }
        let range = tree.syntax().text_range();
        if tree.star_token().is_some() {
            return Import { module_path: segments, source: None, range };
        }
        // `a::{self}` imports `a`.
        if segments.last().is_some_and(|it| it.name == "self") && trees.len() > 1 {
            segments.pop();
        }
        let source = segments.pop();
        Import { module_path: segments, source, range }
    }

    /// What `path` (rustc's segments) names as a module, if it names one.
    fn module(&self, path: &[Segment]) -> Option<Scope> {
        let db = self.cx.db;
        let (kind, rest) = self.kind_of(path);
        if rest.is_empty() {
            let current = self.current()?;
            return match kind {
                PathKind::Abs if self.edition() > Edition::Edition2015 => Some(Scope::ExternPrelude),
                PathKind::Abs | PathKind::Crate => Some(Scope::Def(ModuleDef::Module(current.crate_root(db)))),
                PathKind::Super(n) => {
                    let mut module = current;
                    for _ in 0..n {
                        module = module.parent(db)?;
                    }
                    Some(Scope::Def(ModuleDef::Module(module)))
                }
                _ => None,
            };
        }
        let path = ModPath::from_segments(kind, rest.iter().map(|it| Name::new_root(&it.name)));
        let mut items = self.cx.sema.resolve_mod_path(&self.node, &path)?;
        items.find_map(|it| match it {
            ItemInNs::Types(def @ (ModuleDef::Module(_) | ModuleDef::Trait(_))) => Some(Scope::Def(def)),
            ItemInNs::Types(def @ ModuleDef::Adt(hir::Adt::Enum(_))) => Some(Scope::Def(def)),
            _ => None,
        })
    }

    /// What `name` names as a value in `scope`, or in the import's scope when there is none.
    fn value(&self, scope: Option<&[Segment]>, name: &str) -> Option<ModuleDef> {
        let mut path: Vec<Segment> = scope.map_or_else(Vec::new, <[Segment]>::to_vec);
        path.push(Segment { name: name.to_owned(), range: TextRange::default() });
        let (kind, rest) = self.kind_of(&path);
        let path = ModPath::from_segments(kind, rest.iter().map(|it| Name::new_root(&it.name)));
        let mut items = self.cx.sema.resolve_mod_path(&self.node, &path)?;
        items.find_map(|it| match it {
            ItemInNs::Values(def) => Some(def),
            _ => None,
        })
    }

    /// Whether the import's name is found in the module `scope`, in any namespace.
    fn found_in(&self, import: &Import, source: &Segment) -> bool {
        let mut path = import.module_path.clone();
        path.push(source.clone());
        let (kind, rest) = self.kind_of(&path);
        if rest.is_empty() {
            return true;
        }
        let path = ModPath::from_segments(kind, rest.iter().map(|it| Name::new_root(&it.name)));
        self.cx.sema.resolve_mod_path(&self.node, &path).is_some_and(|mut it| it.next().is_some())
    }

    /// The leading keywords of `path` as hir's path kind, and the names after them.
    fn kind_of<'p>(&self, path: &'p [Segment]) -> (PathKind, &'p [Segment]) {
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

    /// Why `import` fails as rustc's `finalize_import` finds it, or none when it resolves.
    fn failure(&self, import: &Import) -> Option<Failure> {
        let path = &import.module_path;
        let mut module = None;
        for (at, segment) in path.iter().enumerate() {
            match self.module(&path[..=at]) {
                Some(found) => module = Some(found),
                None => {
                    let failed = self.path_error(path, at, module, false);
                    if at + 1 < path.len() {
                        return Some(Failure::Path { at: segment.range, error: failed });
                    }
                    let (label, note, suggestion) = match self.similar_path(path) {
                        Some((suggested, note)) => {
                            let suggested = names_to_string(suggested.iter().map(|it| it.name.as_str()));
                            let parts = vec![(self.span(segment.range), suggested)];
                            (None, note, Some(Suggestion { parts, message: "a similar path exists".to_owned() }))
                        }
                        None => (Some(failed.label), None, failed.suggestion),
                    };
                    return Some(Failure::Import(ImportError {
                        span: segment.range,
                        label_at: segment.range,
                        path: import_path(import, Some(at)),
                        label,
                        note,
                        suggestion,
                        fact: failed.fact,
                    }));
                }
            }
        }
        let source = import.source.as_ref()?;
        if self.found_in(import, source) {
            return None;
        }
        let scope = match module {
            Some(scope) => scope,
            None if path.is_empty() => Scope::ExternPrelude,
            None => return None,
        };
        let label = match scope {
            Scope::Def(def) => match module_to_string(self.cx, def) {
                Some(module) => format!("no `{}` in `{module}`", source.name),
                None => format!("no `{}` in the root", source.name),
            },
            Scope::ExternPrelude if !is_path_segment_keyword(&source.name) => {
                format!("no external crate `{}`", source.name)
            }
            Scope::ExternPrelude => format!("no `{}` in the root", source.name),
        };
        let names = self.names_in(scope, &source.name);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let suggestion = find_best_match_for_name(&names, &source.name, None).map(|it| Suggestion {
            parts: vec![(self.span(source.range), it.to_owned())],
            message: "a similar name exists in the module".to_owned(),
        });
        let fact = match scope {
            Scope::Def(ModuleDef::Module(m)) => Some((
                self.cx.resolves_fact(m, &source.name),
                format!("what `{}` names in `{}`", source.name, module_to_string(self.cx, ModuleDef::Module(m)).unwrap_or_else(|| "crate".to_owned())),
            )),
            _ => None,
        };
        Some(Failure::Import(ImportError {
            span: import.range,
            label_at: source.range,
            path: import_path(import, None),
            label: Some(label),
            note: None,
            suggestion,
            fact,
        }))
    }

    /// The names of `scope` rustc may suggest for `name`: not `name` itself, nor `_`.
    fn names_in(&self, scope: Scope, name: &str) -> Vec<String> {
        let db = self.cx.db;
        let mut names: Vec<String> = match scope {
            Scope::Def(ModuleDef::Module(m)) => m
                .scope(db, None)
                .into_iter()
                .filter(|(_, def)| !matches!(def, hir::ScopeDef::Unknown))
                .map(|(name, _)| name.as_str().to_owned())
                .collect(),
            Scope::Def(ModuleDef::Adt(hir::Adt::Enum(e))) => {
                e.variants(db).into_iter().map(|it| it.name(db).as_str().to_owned()).collect()
            }
            Scope::Def(ModuleDef::Trait(t)) => {
                t.items(db).into_iter().filter_map(|it| it.name(db)).map(|it| it.as_str().to_owned()).collect()
            }
            _ => Vec::new(),
        };
        names.retain(|it| it != name && it != "_");
        names.sort();
        names.dedup();
        names
    }

    /// rustc's `make_path_suggestion`: the path with `self`, `crate`, `super` or a crate's name
    /// first, when that names a module, and the note it gives with it.
    fn similar_path(&self, path: &[Segment]) -> Option<(Vec<Segment>, Option<String>)> {
        let mut path = path.to_vec();
        match path.as_slice() {
            [first, second, ..] if first.name == ROOT && !is_path_segment_keyword(&second.name) => {}
            [first, ..] if self.edition() > Edition::Edition2015 && !is_path_segment_keyword(&first.name) => {
                path.insert(0, Segment { name: String::new(), range: TextRange::default() });
            }
            _ => return None,
        }
        let with = |first: &str| {
            let mut path = path.clone();
            path[0].name = first.to_owned();
            path
        };
        for first in ["self", "crate", "super"] {
            let candidate = with(first);
            if self.module(&candidate).is_some() {
                let note = (first == "crate").then(|| {
                    "`use` statements changed in Rust 2018; read more at <https://doc.rust-lang.org/edition-guide/rust-2018/module-system/path-clarity.html>".to_owned()
                });
                return Some((candidate, note));
            }
        }
        if self.edition() == Edition::Edition2015 {
            return None;
        }
        let mut crates = self.extern_prelude();
        crates.sort_by(|a, b| b.cmp(a));
        crates.into_iter().map(|it| with(&it)).find(|it| self.module(it).is_some()).map(|it| (it, None))
    }

    /// The names of the crate's extern prelude: its dependencies.
    fn extern_prelude(&self) -> Vec<String> {
        self.cx.krate.dependencies(self.cx.db).into_iter().map(|it| it.name.as_str().to_owned()).collect()
    }

    /// rustc's `report_path_resolution_error` of the segment `at` of `path`, `module` what the
    /// segments before it named; `macro_last` when the last segment is looked for as a macro
    /// (an import's path is all of modules).
    fn path_error(&self, path: &[Segment], at: usize, module: Option<Scope>, macro_last: bool) -> PathError {
        let cx = self.cx;
        let db = cx.db;
        let ident = &path[at];
        let name = ident.name.as_str();
        let scope = match path[..at].last() {
            Some(prev) if prev.name == ROOT => "the crate root".to_owned(),
            Some(prev) => format!("`{}`", prev.name),
            None => "this scope".to_owned(),
        };
        let message = format!("cannot find `{name}` in {scope}");
        let in_module = match module {
            Some(Scope::Def(ModuleDef::Module(m))) => Some(m),
            _ => self.current(),
        };
        let fact = in_module.map(|m| {
            let of = module_to_string(cx, ModuleDef::Module(m)).map_or_else(|| "the crate root".to_owned(), |it| format!("`{it}`"));
            (cx.resolves_fact(m, name), format!("what `{name}` names in {of}"))
        });
        let error = |message: String, label: String, suggestion: Option<Suggestion>| PathError {
            message,
            label,
            suggestion,
            fact: fact.clone(),
        };
        if name == "super" {
            return error(
                "too many leading `super` keywords".to_owned(),
                "there are too many leading `super` keywords".to_owned(),
                None,
            );
        }
        let crate_root = cx.krate.root_module(db);
        if module == Some(Scope::Def(ModuleDef::Module(crate_root))) {
            if let Some(candidate) = self.module_candidate(path, at) {
                let parts = vec![(self.span(ident.range), candidate)];
                return error(
                    message,
                    "unresolved import".to_owned(),
                    Some(Suggestion { parts, message: "a similar path exists".to_owned() }),
                );
            }
            if name == "core" {
                let parts = vec![(self.span(ident.range), "std".to_owned())];
                return error(
                    message,
                    "you might be missing crate `core`".to_owned(),
                    Some(Suggestion { parts, message: "try using `std` instead of `core`".to_owned() }),
                );
            }
            if name == "_" {
                return error(
                    "invalid crate or module name `_`".to_owned(),
                    "`_` is not a valid crate or module name".to_owned(),
                    None,
                );
            }
            if self.edition() == Edition::Edition2015 {
                let parts = vec![(self.crate_insert_span(), format!("extern crate {name};\n"))];
                return error(
                    format!("cannot find module or crate `{name}` in {scope}"),
                    format!("use of unresolved module or unlinked crate `{name}`"),
                    Some(Suggestion {
                        parts,
                        message: format!(
                            "you might be missing a crate named `{name}`, add it to your project and import it in your code"
                        ),
                    }),
                );
            }
            return error(message, format!("could not find `{name}` in the crate root"), None);
        }
        if at > 0 {
            let parent = match path[at - 1].name.as_str() {
                ROOT if self.edition() > Edition::Edition2015 => "the list of imported crates".to_owned(),
                ROOT | "crate" => "the crate root".to_owned(),
                parent => format!("`{parent}`"),
            };
            let in_types = !(macro_last && at + 1 == path.len());
            let label = match self.value(Some(&path[..at]), name).filter(|_| in_types) {
                Some(def) => format!("expected type, found {} `{name}` in {parent}", value_descr(cx, def)),
                None => format!("could not find `{name}` in {parent}"),
            };
            return error(message, label, None);
        }
        if name == "Self" {
            return error(message, "`Self` cannot be used in imports".to_owned(), None);
        }
        if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            return error(
                format!("cannot find type `{name}` in {scope}"),
                format!("use of undeclared type `{name}`"),
                None,
            );
        }
        let mut suggestion = (name == "alloc").then(|| Suggestion {
            parts: Vec::new(),
            message: "add `extern crate alloc` to use the `alloc` crate".to_owned(),
        });
        if suggestion.is_none() {
            suggestion = self.similar_module_or_crate(name).map(|it| Suggestion {
                parts: vec![(self.span(ident.range), it)],
                message: "there is a crate or module with a similar name".to_owned(),
            });
        }
        let message = format!("cannot find module or crate `{name}` in {scope}");
        if let Some(def) = self.value(None, name) {
            let label = format!("{} `{name}` is not a crate or module", value_descr(cx, def));
            return error(message, label, suggestion);
        }
        let suggestion = suggestion.or_else(|| {
            Some(Suggestion { parts: Vec::new(), message: format!("you might be missing a crate named `{name}`") })
        });
        error(message, format!("use of unresolved module or unlinked crate `{name}`"), suggestion)
    }

    /// rustc's `lookup_import_candidates` of a module named as the segment `at` of `path`, as
    /// the path that names it with what it shares with `path` left out.
    fn module_candidate(&self, path: &[Segment], at: usize) -> Option<String> {
        let db = self.cx.db;
        let name = path[at].name.as_str();
        let current = self.current()?;
        let mut candidates: Vec<Vec<String>> = self
            .cx
            .krate
            .modules(db)
            .into_iter()
            .filter(|m| m.name(db).is_some_and(|it| it.as_str() == name))
            .filter(|m| m.is_visible_from(db, current))
            .map(|m| {
                let mut segments: Vec<String> = Vec::new();
                if self.edition() > Edition::Edition2015 {
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

    /// rustc's `find_similarly_named_module_or_crate`: a crate of the extern prelude or a
    /// module below the import's module whose name is close to `name`.
    fn similar_module_or_crate(&self, name: &str) -> Option<String> {
        let db = self.cx.db;
        let current = self.current()?;
        let mut candidates = self.extern_prelude();
        for module in self.cx.krate.modules(db) {
            let below = module != current && module.path_to_root(db).contains(&current);
            if below && let Some(it) = module.name(db) {
                candidates.push(it.as_str().to_owned());
            }
        }
        candidates.retain(|it| !it.is_empty());
        candidates.sort();
        candidates.dedup();
        let candidates: Vec<&str> = candidates.iter().map(String::as_str).collect();
        find_best_match_for_name(&candidates, name, None).filter(|it| *it != name).map(str::to_owned)
    }

    /// rustc's `current_crate_outer_attr_insert_span`: before the first token after the crate
    /// root's inner attributes.
    fn crate_insert_span(&self) -> Span {
        let cx = self.cx;
        let file = cx.krate.root_file(cx.db);
        let parse = syntax::SourceFile::parse(cx.file_text(file), cx.edition);
        let root = parse.tree();
        let at = root
            .syntax()
            .descendants_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|token| {
                let kind = token.kind();
                if kind == SyntaxKind::WHITESPACE || kind == SyntaxKind::SHEBANG {
                    return false;
                }
                if kind == SyntaxKind::COMMENT {
                    let text = token.text();
                    let inner_doc = text.starts_with("//!") || text.starts_with("/*!");
                    let outer_doc = (text.starts_with("///") && !text.starts_with("////"))
                        || (text.starts_with("/**") && !text.starts_with("/***") && text != "/**/");
                    return outer_doc && !inner_doc;
                }
                let in_inner_attr = token
                    .parent_ancestors()
                    .filter_map(ast::Attr::cast)
                    .any(|it| it.excl_token().is_some());
                !in_inner_attr
            })
            .map_or(TextSize::of(cx.file_text(file)), |it| it.text_range().start());
        cx.span_in_file(file, TextRange::empty(at))
    }
}

/// What rustc's `report_path_resolution_error` gives.
struct PathError {
    message: String,
    label: String,
    suggestion: Option<Suggestion>,
    fact: Option<(Fact, String)>,
}

/// rustc's `module_to_string`: the path of a module, enum or trait from the crate root, or
/// none for a crate root.
fn module_to_string(cx: &Cx<'_, '_>, def: ModuleDef) -> Option<String> {
    let db = cx.db;
    let (module, own) = match def {
        ModuleDef::Module(m) => (m, None),
        def => (def.module(db)?, def.name(db)),
    };
    let names: Vec<String> =
        module.path_segments(db).chain(own).map(|it| it.as_str().to_owned()).collect();
    (!names.is_empty()).then(|| names_to_string(names.iter().map(String::as_str)))
}

/// rustc's `Res::descr` of what a name means as a value.
fn value_descr(cx: &Cx<'_, '_>, def: ModuleDef) -> &'static str {
    let db = cx.db;
    match def {
        ModuleDef::Function(_) => "function",
        ModuleDef::Const(_) => "constant",
        ModuleDef::Static(_) => "static",
        ModuleDef::Adt(hir::Adt::Struct(it)) => match it.kind(db) {
            StructKind::Tuple => "tuple struct",
            StructKind::Unit => "unit struct",
            StructKind::Record => "struct",
        },
        ModuleDef::EnumVariant(it) => match it.kind(db) {
            StructKind::Tuple => "tuple variant",
            StructKind::Unit => "unit variant",
            StructKind::Record => "struct variant",
        },
        ModuleDef::Macro(_) => "macro",
        _ => "value",
    }
}

/// The segments of a path as written: `::` first as `{{root}}`, keywords as themselves.
pub(super) fn path_segments(path: &ast::Path) -> Vec<Segment> {
    let mut out = Vec::new();
    for (i, segment) in path.segments().enumerate() {
        if i == 0
            && let Some(token) = segment.coloncolon_token()
        {
            out.push(Segment { name: ROOT.to_owned(), range: token.text_range() });
        }
        if let Some(name) = segment.name_ref() {
            out.push(Segment { name: name.text().to_string(), range: name.syntax().text_range() });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(name: &str) -> Segment {
        Segment { name: name.to_owned(), range: TextRange::default() }
    }

    #[test]
    fn the_path_of_an_import_is_as_rustc_writes_it() {
        let import = Import {
            module_path: vec![segment(ROOT), segment("zed")],
            source: Some(segment("baz")),
            range: TextRange::default(),
        };
        assert_eq!(import_path(&import, None), "zed::baz");
        assert_eq!(import_path(&import, Some(1)), "zed");
        let glob = Import { module_path: vec![segment("a"), segment("b")], source: None, range: TextRange::default() };
        assert_eq!(import_path(&glob, None), "a::b::*");
        let single = Import { module_path: vec![segment(ROOT)], source: Some(segment("foo")), range: TextRange::default() };
        assert_eq!(import_path(&single, None), "foo");
    }

    #[test]
    fn names_are_joined_with_the_root_as_nothing() {
        assert_eq!(names_to_string(["self", "foo"]), "self::foo");
        assert_eq!(names_to_string([ROOT, "foo"]), "::foo");
    }
}
