//! `unused_qualifications`: a path of an expression, pattern, struct literal, type or trait
//! whose later segment is what its name alone already finds where the path is written.
//!
//! A segment has a resolution of its own up to the first that is not a module, enum or
//! trait, that one included; the segments after it are left to type checking. The name alone
//! is looked up as hir looks up a one-segment path at the path, locals and generics first.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`lint_unused_qualifications`,
//! `smart_resolve_path_fragment`, `resolve_qpath`), `ident.rs` (`resolve_path_with_ribs`,
//! `record_segment_res`) and `check_unused.rs` (`potentially_unnecessary_qualifications`)
//! (MIT or Apache-2.0).

use hir::{HirFileId, InFile, ModuleDef, PathResolution};
use hir_def::ModuleId;
use hir_expand::{
    mod_path::PathKind,
    name::{AsName, Name},
};
use rustc_hash::FxHashMap;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange,
    ast::{self, HasGenericArgs, make},
};

use super::{
    unused::Unused,
    used::{Ns, Uses, entry},
};
use crate::{
    cx::Cx,
    lints::syntax::Walk,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// rustc's `potentially_unnecessary_qualifications`, less those whose name alone is found
/// through an import found unused or redundant, in the order late resolution comes to them.
pub(super) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>, uses: &Uses, unused: &Unused) -> Vec<Found> {
    let mut found = Vec::new();
    let mut scratch = Scratch::default();
    for tree in &walk.trees {
        for node in &tree.nodes {
            if node.kind() != SyntaxKind::PATH {
                continue;
            }
            let Some(path) = ast::Path::cast(node.clone()) else { continue };
            found.extend(qualification(cx, tree.file, &path, uses, unused, &mut scratch));
        }
    }
    found
}

/// What one path's check needs, kept from path to path: the segments, what each resolves to,
/// and the one-segment path of each name, parsed once.
#[derive(Default)]
struct Scratch<'db> {
    chain: Vec<ast::Path>,
    segments: Vec<(ast::Path, ast::PathSegment)>,
    resolved: Vec<Option<PathResolution<'db>>>,
    alone: FxHashMap<Name, ast::Path>,
}

fn qualification<'db>(
    cx: &Cx<'_, 'db>,
    file: HirFileId,
    path: &ast::Path,
    uses: &Uses,
    unused: &Unused,
    scratch: &mut Scratch<'db>,
) -> Option<Found> {
    if path.qualifier().is_none() || path.parent_path().is_some() {
        return None;
    }
    let parent = path.syntax().parent()?;
    let kind = parent.kind();
    let ns = if ast::PathExpr::can_cast(kind) || ast::PathPat::can_cast(kind) || ast::TupleStructPat::can_cast(kind) {
        Ns::Values
    } else if ast::RecordExpr::can_cast(kind)
        || ast::RecordPat::can_cast(kind)
        || (ast::PathType::can_cast(kind) && !is_qualifying_trait(&parent))
    {
        Ns::Types
    } else {
        return None;
    };
    let Scratch { chain, segments, resolved, alone } = scratch;
    segments_into(path, chain, segments)?;
    if segments.len() < 2 || segments[0].1.coloncolon_token().is_some() {
        return None;
    }
    if !written(cx, file, path.syntax().text_range())
        || segments.iter().any(|(_, segment)| !written(cx, file, segment.syntax().text_range()))
    {
        return None;
    }
    let end = segments.iter().position(|(_, segment)| has_generic_args(segment)).map_or(segments.len(), |it| it + 1);
    resolve_into(cx, &segments[..end], resolved);
    let scope = cx.sema.scope(path.syntax())?;
    let module = ModuleId::from(scope.module());
    let (seg, name_ref, name) = (1..end).rev().find_map(|i| {
        let res = resolved[i]?;
        let name_ref = segments[i].1.name_ref()?;
        let name = name_ref.as_name();
        let probe = alone.entry(name.clone()).or_insert_with(|| make::ext::ident_path(name_ref.text()));
        let lexical = scope.speculative_resolve(probe)?;
        (lexical == res).then_some((i, name_ref, name))
    })?;
    let ns = if seg + 1 == segments.len() { ns } else { Ns::Types };
    let found_through =
        entry(&uses.lookup(cx, module, PathKind::Plain, std::slice::from_ref(&name)), ns).and_then(|it| it.import);
    if found_through.is_some_and(|it| unused.unused.contains(&it) || unused.redundant.contains(&it)) {
        return None;
    }
    let start = segments[0].1.syntax().text_range().start();
    let removal = TextRange::new(start, segments[seg].1.syntax().text_range().start());
    let mut diag = Diag::lint("unused_qualifications", "unnecessary qualification", cx.span(InFile::new(file, path.syntax().text_range())));
    diag.suggestion_verbose(
        cx.span(InFile::new(file, removal)),
        "remove the unnecessary path segments",
        "",
        Applicability::MachineApplicable,
    );
    let text = name_ref.text();
    diag.fact(cx.resolves_fact(module.into(), text), format!("what `{text}` alone finds"));
    Some(Found::lint(diag, Phase::Resolve, InFile::new(file, SyntaxNodePtr::new(&parent))))
}

/// rustc's segments of `path` into `segments`, each with the path it ends: those of the trait
/// of `<T as Trait>::a` then the rest; of `<T>::a`, the rest.
fn segments_into(
    path: &ast::Path,
    chain: &mut Vec<ast::Path>,
    segments: &mut Vec<(ast::Path, ast::PathSegment)>,
) -> Option<()> {
    segments.clear();
    chain_into(path, chain);
    let first = chain[0].segment()?;
    let rest = match first.type_anchor() {
        Some(anchor) => {
            if anchor.as_token().is_some() {
                let mut inner = Vec::new();
                chain_into(&first.qualifying_trait()?.path()?, &mut inner);
                if inner[0].segment()?.type_anchor().is_some() {
                    return None;
                }
                for path in inner {
                    let segment = path.segment()?;
                    segments.push((path, segment));
                }
            }
            1
        }
        None => 0,
    };
    for path in chain.drain(rest..) {
        let segment = path.segment()?;
        segments.push((path, segment));
    }
    Some(())
}

/// `path` and its qualifiers into `chain`, outermost qualifier first.
fn chain_into(path: &ast::Path, chain: &mut Vec<ast::Path>) {
    chain.clear();
    chain.push(path.clone());
    while let Some(qualifier) = chain.last().and_then(|it| it.qualifier()) {
        chain.push(qualifier);
    }
    chain.reverse();
}

/// What each segment resolves to into `resolved`, up to the first that is not a module, enum
/// or trait.
fn resolve_into<'db>(
    cx: &Cx<'_, 'db>,
    segments: &[(ast::Path, ast::PathSegment)],
    resolved: &mut Vec<Option<PathResolution<'db>>>,
) {
    resolved.clear();
    let mut open = true;
    for (path, _) in segments {
        if !open {
            resolved.push(None);
            continue;
        }
        let res = cx.sema.resolve_path(path);
        open = matches!(
            res,
            Some(PathResolution::Def(
                ModuleDef::Module(_) | ModuleDef::Adt(hir::Adt::Enum(_)) | ModuleDef::Trait(_)
            ))
        );
        resolved.push(res);
    }
}

/// rustc's `Segment::has_generic_args`: `::<..>`, `(..)` or `(..)` sugar's return type.
fn has_generic_args(segment: &ast::PathSegment) -> bool {
    segment.generic_arg_list().is_some()
        || segment.parenthesized_arg_list().is_some()
        || segment.ret_type().is_some()
        || segment.return_type_syntax().is_some()
}

/// The trait of `<T as Trait>`, which rustc resolves with the path it qualifies.
fn is_qualifying_trait(ty: &SyntaxNode) -> bool {
    ty.parent().and_then(ast::TypeAnchor::cast).is_some_and(|anchor| {
        anchor.as_token().is_some()
            && anchor
                .syntax()
                .parent()
                .and_then(ast::PathSegment::cast)
                .and_then(|it| it.qualifying_trait())
                .is_some_and(|it| it.syntax() == ty)
    })
}

/// Whether `range` is written by the user, not by a macro: rustc's `!span.from_expansion()`.
fn written(cx: &Cx<'_, '_>, file: HirFileId, range: TextRange) -> bool {
    if file.macro_file().is_none() {
        return true;
    }
    InFile::new(file, range).original_node_file_range_opt(cx.db).is_some_and(|(_, ctxt)| ctxt.is_root())
}
